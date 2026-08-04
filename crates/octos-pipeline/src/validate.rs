//! Graph validation (lint rules) for pipelines.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

use crate::condition;
use crate::graph::{HandlerKind, PipelineEdge, PipelineGraph, PipelineNode};

/// Default static cap for authored fan-out edges. Runtime fan-out has a
/// separate cumulative guard in the executor; this catches obviously malformed
/// DOT before dispatch.
pub const MAX_DECLARED_FANOUT_DEGREE: usize = 50;

/// A validation diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintDiagnostic {
    /// Backwards-compatible numeric rule id used by existing logs/tests.
    pub rule: u32,
    /// Typed rule id used by new pre-execution diagnostics.
    pub rule_id: RuleId,
    pub severity: Severity,
    pub location: GraphLocation,
    pub message: String,
    pub fix_hint: Option<String>,
}

/// Preferred diagnostic name for new call sites.
pub type PipelineDiagnostic = LintDiagnostic;

/// Diagnostic severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// Location of a graph validation diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphLocation {
    Graph,
    Node(String),
    Edge { source: String, target: String },
}

/// Typed validation rule identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleId {
    StartNode,
    Connectivity,
    EdgeTargetExists,
    NoSelfLoop,
    GoalGateEdges,
    ConditionParse,
    KnownHandler,
    PromptRequired,
    NoDuplicateEdges,
    PositiveWeight,
    NonEmptyGraph,
    EdgeSourceExists,
    ParallelConverge,
    DynamicParallel,
    NoCycle,
    TemplateBinding,
    DeadEdge,
    KnownModel,
    KnownToolPolicy,
    FanoutBound,
    HumanGateResolver,
    ReferenceResolution,
    NoShell,
}

impl RuleId {
    pub fn number(self) -> u32 {
        match self {
            Self::StartNode => 1,
            Self::Connectivity => 2,
            Self::EdgeTargetExists => 3,
            Self::NoSelfLoop => 4,
            Self::GoalGateEdges => 5,
            Self::ConditionParse => 6,
            Self::KnownHandler => 7,
            Self::PromptRequired => 8,
            Self::NoDuplicateEdges => 9,
            Self::PositiveWeight => 10,
            Self::NonEmptyGraph => 11,
            Self::EdgeSourceExists => 12,
            Self::ParallelConverge => 13,
            Self::DynamicParallel => 14,
            Self::NoCycle => 15,
            Self::TemplateBinding => 16,
            Self::DeadEdge => 17,
            Self::KnownModel => 18,
            Self::KnownToolPolicy => 19,
            Self::FanoutBound => 20,
            Self::HumanGateResolver => 21,
            Self::ReferenceResolution => 22,
            Self::NoShell => 23,
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::TemplateBinding => "T-Agent",
            Self::DeadEdge => "T-Edge",
            Self::Connectivity | Self::NoCycle | Self::NoSelfLoop => "T-Conn",
            Self::KnownModel => "OCTOS-Model",
            Self::KnownToolPolicy => "OCTOS-ToolPolicy",
            Self::FanoutBound => "OCTOS-Fanout",
            Self::HumanGateResolver => "OCTOS-HumanGate",
            Self::ReferenceResolution => "OCTOS-Ref",
            _ => "PipelineLint",
        }
    }
}

/// Pure validation context supplied by the caller.
///
/// The validator never reads the filesystem, calls a provider, or constructs
/// tools. Runtime state such as the model catalog or tool registry is projected
/// into this struct before validation starts.
#[derive(Debug, Clone)]
pub struct ValidationContext {
    pub known_models: BTreeSet<String>,
    pub known_tools: BTreeSet<String>,
    pub runtime_variables: BTreeSet<String>,
    pub artifact_refs: BTreeSet<String>,
    pub checkpoint_refs: BTreeSet<String>,
    pub human_gate_resolvers: BTreeSet<String>,
    pub max_fanout_degree: usize,
}

impl Default for ValidationContext {
    fn default() -> Self {
        Self {
            known_models: default_known_models(),
            known_tools: default_known_tools(),
            runtime_variables: default_runtime_variables(),
            artifact_refs: BTreeSet::new(),
            checkpoint_refs: BTreeSet::new(),
            human_gate_resolvers: BTreeSet::new(),
            max_fanout_degree: MAX_DECLARED_FANOUT_DEGREE,
        }
    }
}

impl ValidationContext {
    pub fn with_runtime_variables<I, S>(mut self, variables: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.runtime_variables
            .extend(variables.into_iter().map(Into::into));
        self
    }

    /// Restrict the known-model set to what the provider router can ACTUALLY
    /// resolve (#1901, layer 5).
    ///
    /// `with_known_models` is fed from the model CATALOG, which lists every
    /// model the profile could theoretically use — 93 of them — regardless of
    /// what the router registered. So a graph naming a catalogued-but-
    /// unregistered model PASSED validation and then failed at runtime: the
    /// pre-flight check certified a graph that cannot run. Worse, the rejection
    /// message points at `model_catalog.json`, which is precisely where the
    /// model IS present — sending the reader to the wrong file.
    ///
    /// Anything the router registers is resolvable, including the lane keys
    /// (`cheap`/`strong`), so router keys are the correct authority. Callers
    /// without a router (or with an empty one) keep the catalog-based set
    /// rather than rejecting everything — validation must not become stricter
    /// than reality when it has no way to know.
    pub fn with_resolvable_models<I, S>(mut self, registered: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let registered: BTreeSet<String> = registered.into_iter().map(Into::into).collect();
        if registered.is_empty() {
            return self;
        }
        self.known_models = registered;
        self
    }

    pub fn with_known_models<I, S>(mut self, models: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.known_models
            .extend(models.into_iter().map(|m| m.into()));
        self
    }

    pub fn with_known_tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.known_tools.extend(tools.into_iter().map(Into::into));
        self
    }
}

/// Validate a pipeline graph against all rules.
///
/// Returns `Ok(())` when no error-severity diagnostics were emitted, or every
/// diagnostic when at least one error is present.
pub fn validate(graph: &PipelineGraph) -> Result<(), Vec<PipelineDiagnostic>> {
    validate_with_context(graph, &ValidationContext::default())
}

/// Validate using caller-provided pure runtime facts.
pub fn validate_with_context(
    graph: &PipelineGraph,
    context: &ValidationContext,
) -> Result<(), Vec<PipelineDiagnostic>> {
    let diags = diagnostics_with_context(graph, context);
    if has_errors(&diags) {
        Err(diags)
    } else {
        Ok(())
    }
}

/// Return diagnostics without converting error-severity diagnostics into
/// `Result::Err`. This is useful for logging warnings before bailing.
pub fn diagnostics(graph: &PipelineGraph) -> Vec<PipelineDiagnostic> {
    diagnostics_with_context(graph, &ValidationContext::default())
}

/// Return diagnostics using caller-provided pure runtime facts.
pub fn diagnostics_with_context(
    graph: &PipelineGraph,
    context: &ValidationContext,
) -> Vec<PipelineDiagnostic> {
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
    rule_16_template_bindings(graph, context, &mut diags);
    rule_17_dead_edges(graph, &mut diags);
    rule_18_known_models(graph, context, &mut diags);
    rule_19_known_tools(graph, context, &mut diags);
    rule_20_fanout_bound(graph, context, &mut diags);
    rule_21_human_gate_resolver(graph, context, &mut diags);
    rule_22_reference_resolution(graph, context, &mut diags);
    rule_23_no_shell(graph, &mut diags);
    diags
}

/// Rule 23 — the `shell` handler and shell-family tools (`shell`/`bash`/`exec`/
/// `exec_command`) are NOT permitted in ANY pipeline (LLM-authored or
/// operator-authored DOT). Shell is arbitrary code execution; the unsafe DOT
/// authoring surface is removed at the validation gate that every execution +
/// pre-flight path runs through. Pipelines use the capability-locked handlers
/// (codergen/gate/noop/parallel/dynamic_parallel) with explicit, non-shell tools.
fn rule_23_no_shell(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    use crate::graph::HandlerKind;
    for node in graph.nodes.values() {
        if node.handler == HandlerKind::Shell {
            push_diag(
                diags,
                RuleId::NoShell,
                Severity::Error,
                GraphLocation::Node(node.id.clone()),
                format!(
                    "node '{}' uses the `shell` handler, which is not permitted in pipelines",
                    node.id
                ),
                Some(
                    "shell is arbitrary code execution and is banned; use a codergen/gate/noop handler"
                        .into(),
                ),
            );
        }
        if let Some(tool) = node.tools.iter().find(|t| tool_entry_grants_shell(t)) {
            push_diag(
                diags,
                RuleId::NoShell,
                Severity::Error,
                GraphLocation::Node(node.id.clone()),
                format!(
                    "node '{}' grants shell access via the tool entry '{tool}', which is not permitted",
                    node.id
                ),
                Some(
                    "use explicit non-shell tool names; remove shell/bash/exec/exec_command/\
                     write_stdin, wildcards (`*`), and `group:runtime`/`group:*`"
                        .into(),
                ),
            );
        }
    }
}

/// True when a node's `tools` entry grants shell access — directly (a
/// shell-family tool name) or INDIRECTLY through a policy entry the tool-policy
/// layer expands to include shell: the runtime group (`group:runtime` /
/// `group:*`), or a tool-name PREFIX wildcard (`ToolPolicy` treats `prefix*` as
/// a prefix match) whose prefix matches a shell tool — `*`, `exec*`, `sh*`. Safe
/// prefixes (`read_*`, `my_plugin_*`) and safe groups (`group:fs`/`search`/...)
/// match no shell tool and stay allowed.
fn tool_entry_grants_shell(entry: &str) -> bool {
    let e = entry.trim();
    if crate::profile::SHELL_TOOLS.contains(&e) {
        return true;
    }
    if e.eq_ignore_ascii_case("group:runtime") || e == "group:*" {
        return true;
    }
    if !e.starts_with("group:") {
        if let Some(prefix) = e.strip_suffix('*') {
            return prefix.is_empty()
                || crate::profile::SHELL_TOOLS
                    .iter()
                    .any(|s| s.starts_with(prefix));
        }
    }
    false
}

/// Check if any diagnostics are errors.
pub fn has_errors(diags: &[PipelineDiagnostic]) -> bool {
    diags.iter().any(|d| d.severity == Severity::Error)
}

/// Collect every tool/policy entry referenced by any node in the graph.
/// Used to decide whether plugin loading is needed for Rule 19 validation.
pub fn referenced_tool_entries(graph: &PipelineGraph) -> Vec<String> {
    graph
        .nodes
        .values()
        .flat_map(|n| n.tools.iter().cloned())
        .collect()
}

/// Build the set of known tool names for Rule 19 validation: the built-in
/// tools, plus — ONLY when the graph actually references a non-built-in tool —
/// the names the real `PluginLoader` would register from `plugin_dirs`.
///
/// Shared by both validation entry paths (`RunPipelineTool::pre_flight_validate`
/// and `PipelineExecutor::validation_context`) so they cannot drift.
///
/// Design (reconciling codex pre-merge rounds 4–6, which pulled in opposite
/// directions — "don't load, it's slow" vs "a manifest-only scan diverges from
/// what actually registers"):
///
/// - **Built-in-only graphs (the common case):** every referenced tool is
///   already a built-in / `group:` / a wildcard a built-in matches, so we
///   NEVER touch `plugin_dirs` — zero plugin I/O (round-4's perf concern).
/// - **A graph references a tool the built-ins don't cover:** we run the REAL
///   `PluginLoader` and use exactly the tools it registers. That is ground
///   truth — it honours signing (`require_signed`), skips broken/no-exe
///   installs, and matches the executor's plugin cache — so Rule 19 can never
///   diverge from runtime registration in either direction (rounds 5 & 6).
///   The cost is paid only when a plugin tool is genuinely in play.
pub fn known_tool_names_with_plugins(
    working_dir: &std::path::Path,
    plugin_dirs: &[std::path::PathBuf],
    plugin_require_signed: bool,
    referenced_tools: &[String],
) -> Vec<String> {
    let builtins = octos_agent::ToolRegistry::with_builtins(working_dir).tool_names();
    if plugin_dirs.is_empty() {
        return builtins;
    }
    // Does the graph reference anything the built-ins (or group: policy
    // entries) don't already cover? If not, plugin loading cannot change the
    // validation outcome — skip it entirely.
    let builtin_set: std::collections::HashSet<&str> =
        builtins.iter().map(String::as_str).collect();
    let needs_plugins = referenced_tools.iter().any(|t| {
        let t = t.trim();
        if t.is_empty() || t.starts_with("group:") {
            return false;
        }
        // codex round-7 P2: a wildcard (`my_plugin_*`) needs plugin discovery
        // UNLESS a built-in already matches the prefix — otherwise Rule 19
        // would reject a legitimate plugin-prefix policy the executor loads.
        if let Some(prefix) = t.strip_suffix('*') {
            return !builtin_set.iter().any(|b| b.starts_with(prefix));
        }
        !builtin_set.contains(t)
    });
    if !needs_plugins {
        return builtins;
    }
    // Real load = ground truth (signing-aware, skips broken installs), so
    // Rule 19 matches what the executor's plugin cache will register.
    let mut registry = octos_agent::ToolRegistry::with_builtins(working_dir);
    let _ = octos_agent::PluginLoader::load_into_with_options(
        &mut registry,
        plugin_dirs,
        &[],
        octos_agent::PluginLoadOptions {
            work_dir: None,
            synthesis_config: None,
            require_signed: plugin_require_signed,
            verified_cache_dir: None,
        },
    );
    registry.tool_names()
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

fn push_diag(
    diags: &mut Vec<PipelineDiagnostic>,
    rule_id: RuleId,
    severity: Severity,
    location: GraphLocation,
    message: impl Into<String>,
    fix_hint: impl Into<Option<String>>,
) {
    diags.push(PipelineDiagnostic {
        rule: rule_id.number(),
        rule_id,
        severity,
        location,
        message: message.into(),
        fix_hint: fix_hint.into(),
    });
}

// ---- Individual rules ----

fn rule_01_start_node(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    if find_start_node(graph).is_none() {
        let incoming: HashSet<&str> = graph.edges.iter().map(|e| e.target.as_str()).collect();
        let sources: Vec<&str> = graph
            .nodes
            .keys()
            .filter(|id| !incoming.contains(id.as_str()))
            .map(String::as_str)
            .collect();

        let message = if sources.is_empty() {
            "no start node found (all nodes have incoming edges)".into()
        } else {
            format!(
                "ambiguous start: {} nodes with no incoming edges: {}",
                sources.len(),
                sources.join(", ")
            )
        };
        push_diag(
            diags,
            RuleId::StartNode,
            Severity::Error,
            GraphLocation::Graph,
            message,
            Some("name the first node `start` or connect the extra source nodes".into()),
        );
    }
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
            push_diag(
                diags,
                RuleId::Connectivity,
                Severity::Error,
                GraphLocation::Node(id.clone()),
                format!("node '{id}' is unreachable from start"),
                Some(format!(
                    "connect '{id}' to a source path or remove the orphan node"
                )),
            );
        }
    }
}

fn rule_03_edge_targets_exist(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    for edge in &graph.edges {
        if !graph.nodes.contains_key(&edge.target) {
            push_diag(
                diags,
                RuleId::EdgeTargetExists,
                Severity::Error,
                edge_location(edge),
                format!("edge target '{}' does not exist", edge.target),
                Some(format!(
                    "declare node '{}' or correct the edge target",
                    edge.target
                )),
            );
        }
    }
}

fn rule_04_no_self_loops(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    for edge in &graph.edges {
        if edge.source == edge.target && !edge_allows_back_edge(edge, graph) {
            push_diag(
                diags,
                RuleId::NoSelfLoop,
                Severity::Error,
                edge_location(edge),
                format!(
                    "self-loop on '{}' without an explicit retry/back-edge marker",
                    edge.source
                ),
                Some("add a retry/back_edge condition or remove the self-loop".into()),
            );
        }
    }
}

fn rule_05_goal_gate_edges(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    for node in graph.nodes.values() {
        if node.goal_gate {
            let has_outgoing = graph.edges.iter().any(|e| e.source == node.id);
            if !has_outgoing {
                push_diag(
                    diags,
                    RuleId::GoalGateEdges,
                    Severity::Warning,
                    GraphLocation::Node(node.id.clone()),
                    format!(
                        "goal_gate node '{}' has no outgoing edges (will always terminate)",
                        node.id
                    ),
                    Some(
                        "add an outgoing failure/retry edge if termination is not intended".into(),
                    ),
                );
            }
        }
    }
}

fn rule_06_conditions_parse(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    for edge in &graph.edges {
        if let Some(ref cond) = edge.condition {
            if let Err(e) = condition::parse_condition(cond) {
                push_diag(
                    diags,
                    RuleId::ConditionParse,
                    Severity::Error,
                    edge_location(edge),
                    format!(
                        "edge {} -> {}: invalid condition '{}': {}",
                        edge.source, edge.target, cond, e
                    ),
                    Some(
                        "rewrite the condition using the pipeline condition expression grammar"
                            .into(),
                    ),
                );
            }
        }
    }
}

fn rule_07_known_handler(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    let _ = (graph, diags);
}

fn rule_08_prompt_required(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    for node in graph.nodes.values() {
        if node.handler == HandlerKind::Codergen && node.prompt.is_none() {
            push_diag(
                diags,
                RuleId::PromptRequired,
                Severity::Warning,
                GraphLocation::Node(node.id.clone()),
                format!(
                    "codergen node '{}' has no prompt (will use default worker prompt)",
                    node.id
                ),
                Some("add a prompt attribute so the node's intent is explicit".into()),
            );
        }
    }
}

fn rule_09_no_duplicate_edges(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    let mut seen = HashSet::new();
    for edge in &graph.edges {
        let key = (&edge.source, &edge.target);
        if !seen.insert(key) {
            push_diag(
                diags,
                RuleId::NoDuplicateEdges,
                Severity::Warning,
                edge_location(edge),
                format!("duplicate edge from '{}' to '{}'", edge.source, edge.target),
                Some(
                    "remove the duplicate edge or add a condition that makes the edge distinct"
                        .into(),
                ),
            );
        }
    }
}

fn rule_10_positive_weight(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    for edge in &graph.edges {
        if edge.weight <= 0.0 {
            push_diag(
                diags,
                RuleId::PositiveWeight,
                Severity::Error,
                edge_location(edge),
                format!(
                    "edge {} -> {}: weight must be positive, got {}",
                    edge.source, edge.target, edge.weight
                ),
                Some("set weight to a positive number".into()),
            );
        }
    }
}

fn rule_11_at_least_one_node(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    if graph.nodes.is_empty() {
        push_diag(
            diags,
            RuleId::NonEmptyGraph,
            Severity::Error,
            GraphLocation::Graph,
            "graph has no nodes",
            Some("declare at least one node".into()),
        );
    }
}

fn rule_12_edge_sources_exist(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    for edge in &graph.edges {
        if !graph.nodes.contains_key(&edge.source) {
            push_diag(
                diags,
                RuleId::EdgeSourceExists,
                Severity::Error,
                edge_location(edge),
                format!("edge source '{}' does not exist", edge.source),
                Some(format!(
                    "declare node '{}' or correct the edge source",
                    edge.source
                )),
            );
        }
    }
}

fn rule_13_parallel_converge(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    for node in graph.nodes.values() {
        if node.handler != HandlerKind::Parallel {
            continue;
        }

        match &node.converge {
            None => {
                push_diag(
                    diags,
                    RuleId::ParallelConverge,
                    Severity::Error,
                    GraphLocation::Node(node.id.clone()),
                    format!("parallel node '{}' missing converge attribute", node.id),
                    Some("set converge to the merge node id".into()),
                );
            }
            Some(target) if !graph.nodes.contains_key(target) => {
                push_diag(
                    diags,
                    RuleId::ParallelConverge,
                    Severity::Error,
                    GraphLocation::Node(node.id.clone()),
                    format!(
                        "parallel node '{}' converge target '{}' does not exist",
                        node.id, target
                    ),
                    Some(format!("declare converge target '{target}'")),
                );
            }
            _ => {}
        }

        let has_targets = graph.edges.iter().any(|e| e.source == node.id);
        if !has_targets {
            push_diag(
                diags,
                RuleId::ParallelConverge,
                Severity::Warning,
                GraphLocation::Node(node.id.clone()),
                format!("parallel node '{}' has no outgoing edges", node.id),
                Some("add fan-out edges or change the handler".into()),
            );
        }
    }
}

fn rule_14_dynamic_parallel(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    for node in graph.nodes.values() {
        if node.handler != HandlerKind::DynamicParallel {
            continue;
        }

        match &node.converge {
            None => {
                push_diag(
                    diags,
                    RuleId::DynamicParallel,
                    Severity::Error,
                    GraphLocation::Node(node.id.clone()),
                    format!(
                        "dynamic_parallel node '{}' missing converge attribute",
                        node.id
                    ),
                    Some("set converge to the merge node id".into()),
                );
            }
            Some(target) if !graph.nodes.contains_key(target) => {
                push_diag(
                    diags,
                    RuleId::DynamicParallel,
                    Severity::Error,
                    GraphLocation::Node(node.id.clone()),
                    format!(
                        "dynamic_parallel node '{}' converge target '{}' does not exist",
                        node.id, target
                    ),
                    Some(format!("declare converge target '{target}'")),
                );
            }
            _ => {}
        }

        if node.prompt.is_none() {
            push_diag(
                diags,
                RuleId::DynamicParallel,
                Severity::Warning,
                GraphLocation::Node(node.id.clone()),
                format!(
                    "dynamic_parallel node '{}' has no prompt (will use default planning prompt)",
                    node.id
                ),
                Some("add a planning prompt for the dynamic fan-out".into()),
            );
        }
    }
}

fn rule_15_no_cycles(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    if let Err(cycle_path) = detect_cycles_ignoring_marked_back_edges(graph) {
        push_diag(
            diags,
            RuleId::NoCycle,
            Severity::Error,
            GraphLocation::Graph,
            cycle_path,
            Some("remove the cycle or mark the retry/guard back-edge explicitly".into()),
        );
    }
}

fn rule_16_template_bindings(
    graph: &PipelineGraph,
    context: &ValidationContext,
    diags: &mut Vec<PipelineDiagnostic>,
) {
    let incoming = incoming_sources(graph);
    for node in graph.nodes.values() {
        let upstream = incoming.get(&node.id).cloned().unwrap_or_default();
        for reference in template_refs_for_node(node) {
            if reference_resolves_in_template(&reference, &upstream, context, graph) {
                continue;
            }
            push_diag(
                diags,
                RuleId::TemplateBinding,
                Severity::Error,
                GraphLocation::Node(node.id.clone()),
                format!(
                    "node '{}' prompt references unbound template variable '{{{}}}'",
                    node.id, reference
                ),
                Some(
                    "bind the variable via runtime inputs, an upstream node output, or a known runtime channel"
                        .into(),
                ),
            );
        }
    }
}

fn rule_17_dead_edges(graph: &PipelineGraph, diags: &mut Vec<PipelineDiagnostic>) {
    for edge in &graph.edges {
        let Some(target) = graph.nodes.get(&edge.target) else {
            continue;
        };
        let refs = template_refs_for_node(target);
        let consumed = refs.iter().any(|reference| {
            is_ref_to_node(reference, &edge.source)
                || runtime_ref_stem(reference)
                    .is_some_and(|stem| matches!(stem, "input" | "previous" | "context"))
        });
        if !consumed {
            push_diag(
                diags,
                RuleId::DeadEdge,
                Severity::Warning,
                edge_location(edge),
                format!(
                    "edge {} -> {} is not referenced by the target template",
                    edge.source, edge.target
                ),
                Some(format!(
                    "reference '{{{}}}' or '{{input}}' in node '{}' if this edge carries data",
                    edge.source, edge.target
                )),
            );
        }
    }
}

fn rule_18_known_models(
    graph: &PipelineGraph,
    context: &ValidationContext,
    diags: &mut Vec<PipelineDiagnostic>,
) {
    if let Some(default_model) = graph.default_model.as_deref() {
        validate_model_name(default_model, context, GraphLocation::Graph, diags);
    }

    for node in graph.nodes.values() {
        for model in node
            .model
            .iter()
            .chain(node.planner_model.iter())
            .flat_map(|models| models.split(','))
            .map(str::trim)
            .filter(|model| !model.is_empty())
        {
            validate_model_name(model, context, GraphLocation::Node(node.id.clone()), diags);
        }
    }
}

fn rule_19_known_tools(
    graph: &PipelineGraph,
    context: &ValidationContext,
    diags: &mut Vec<PipelineDiagnostic>,
) {
    for node in graph.nodes.values() {
        for tool in &node.tools {
            // codex pre-merge P2 (round-2): an empty entry is the explicit
            // `tools=""` deny-all SENTINEL the parser preserves (so the handler
            // can emit `deny: ["*"]`). It is not a tool name, so skip the
            // known-tool check — otherwise Rule 19 errors on the very syntax
            // the P1 fix restored and the text-only node fails validation
            // before the handler ever interprets the sentinel.
            if tool.trim().is_empty() {
                continue;
            }
            if tool_policy_entry_known(tool, context) {
                continue;
            }
            push_diag(
                diags,
                RuleId::KnownToolPolicy,
                Severity::Error,
                GraphLocation::Node(node.id.clone()),
                format!(
                    "node '{}' references unknown tool/group '{}'",
                    node.id, tool
                ),
                Some("use a registered tool name or one of the known group:* policy groups".into()),
            );
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
    for (source, degree) in outgoing {
        if degree > context.max_fanout_degree {
            push_diag(
                diags,
                RuleId::FanoutBound,
                Severity::Error,
                GraphLocation::Node(source.to_string()),
                format!(
                    "node '{}' declares fan-out degree {} above cap {}",
                    source, degree, context.max_fanout_degree
                ),
                Some(
                    "split the fan-out into bounded batches or lower the outgoing edge count"
                        .into(),
                ),
            );
        }
    }

    for node in graph.nodes.values() {
        if let Some(max_tasks) = node.max_tasks {
            if max_tasks as usize > context.max_fanout_degree {
                push_diag(
                    diags,
                    RuleId::FanoutBound,
                    Severity::Error,
                    GraphLocation::Node(node.id.clone()),
                    format!(
                        "node '{}' declares max_tasks {} above cap {}",
                        node.id, max_tasks, context.max_fanout_degree
                    ),
                    Some(
                        "lower max_tasks or raise the validation context cap intentionally".into(),
                    ),
                );
            }
        }
    }
}

fn rule_21_human_gate_resolver(
    graph: &PipelineGraph,
    context: &ValidationContext,
    diags: &mut Vec<PipelineDiagnostic>,
) {
    for node in graph.nodes.values() {
        if !node.human_gate {
            continue;
        }
        let Some(resolver) = node
            .resolver
            .as_deref()
            .filter(|resolver| !resolver.is_empty())
        else {
            push_diag(
                diags,
                RuleId::HumanGateResolver,
                Severity::Error,
                GraphLocation::Node(node.id.clone()),
                format!("human-gate node '{}' has no resolver configured", node.id),
                Some("set resolver or gate_resolver on the human-gate node".into()),
            );
            continue;
        };
        if !context.human_gate_resolvers.is_empty()
            && !context.human_gate_resolvers.contains(resolver)
        {
            push_diag(
                diags,
                RuleId::HumanGateResolver,
                Severity::Error,
                GraphLocation::Node(node.id.clone()),
                format!(
                    "human-gate node '{}' references unknown resolver '{}'",
                    node.id, resolver
                ),
                Some("use one of the configured human gate resolvers".into()),
            );
        }
    }
}

fn rule_22_reference_resolution(
    graph: &PipelineGraph,
    context: &ValidationContext,
    diags: &mut Vec<PipelineDiagnostic>,
) {
    let checkpoint_names = graph_checkpoint_names(graph, context);
    for node in graph.nodes.values() {
        for artifact in &node.artifact_refs {
            if artifact_ref_resolves(artifact, context, graph) {
                continue;
            }
            push_diag(
                diags,
                RuleId::ReferenceResolution,
                Severity::Error,
                GraphLocation::Node(node.id.clone()),
                format!(
                    "node '{}' references dangling artifact '{}'",
                    node.id, artifact
                ),
                Some("reference an upstream node artifact or a known runtime artifact".into()),
            );
        }

        for checkpoint in &node.checkpoint_refs {
            if checkpoint_names.contains(checkpoint) {
                continue;
            }
            push_diag(
                diags,
                RuleId::ReferenceResolution,
                Severity::Error,
                GraphLocation::Node(node.id.clone()),
                format!(
                    "node '{}' references dangling checkpoint '{}'",
                    node.id, checkpoint
                ),
                Some(
                    "declare the checkpoint on a node or pass it in the validation context".into(),
                ),
            );
        }
    }
}

fn validate_model_name(
    model: &str,
    context: &ValidationContext,
    location: GraphLocation,
    diags: &mut Vec<PipelineDiagnostic>,
) {
    if model_known(model, &context.known_models) {
        return;
    }
    push_diag(
        diags,
        RuleId::KnownModel,
        Severity::Error,
        location,
        format!("unknown pipeline model '{model}'"),
        // Deliberately does NOT say "present in model_catalog.json": once the
        // set is router-derived, the rejected model is very often IN the
        // catalog and simply not registered — sending the reader there is the
        // single most misleading thing this message could do (#1901 layer 4).
        Some(format!(
            "'{model}' is not resolvable by this profile's provider router. \
             Use a registered lane (e.g. `strong`, `cheap`) or a model the \
             profile actually registers; known: [{}]",
            context
                .known_models
                .iter()
                .take(12)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )),
    );
}

fn model_known(model: &str, known_models: &BTreeSet<String>) -> bool {
    known_models.contains(model)
        || model
            .split_once('/')
            .is_some_and(|(_, suffix)| known_models.contains(suffix))
}

fn tool_policy_entry_known(entry: &str, context: &ValidationContext) -> bool {
    if entry.is_empty() {
        return false;
    }
    if entry.starts_with("group:") {
        return octos_agent::tools::policy::tool_group_info(entry).is_some();
    }
    if let Some(prefix) = entry.strip_suffix('*') {
        return context
            .known_tools
            .iter()
            .any(|tool| tool.starts_with(prefix));
    }
    context.known_tools.contains(entry)
}

fn edge_location(edge: &PipelineEdge) -> GraphLocation {
    GraphLocation::Edge {
        source: edge.source.clone(),
        target: edge.target.clone(),
    }
}

fn incoming_sources(graph: &PipelineGraph) -> HashMap<String, BTreeSet<String>> {
    let mut incoming: HashMap<String, BTreeSet<String>> = HashMap::new();
    for edge in &graph.edges {
        incoming
            .entry(edge.target.clone())
            .or_default()
            .insert(edge.source.clone());
    }
    incoming
}

fn template_refs_for_node(node: &PipelineNode) -> BTreeSet<String> {
    node.prompt
        .iter()
        .chain(node.worker_prompt.iter())
        .flat_map(|prompt| extract_template_refs(prompt))
        .collect()
}

fn extract_template_refs(prompt: &str) -> BTreeSet<String> {
    let mut refs = BTreeSet::new();
    let mut rest = prompt;
    while let Some(start) = rest.find('{') {
        rest = &rest[start + 1..];
        let Some(end) = rest.find('}') else {
            break;
        };
        let candidate = rest[..end].trim();
        if is_template_ref_token(candidate) {
            refs.insert(candidate.to_string());
        }
        rest = &rest[end + 1..];
    }
    refs
}

fn is_template_ref_token(candidate: &str) -> bool {
    !candidate.is_empty()
        && candidate
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
}

fn reference_resolves_in_template(
    reference: &str,
    upstream: &BTreeSet<String>,
    context: &ValidationContext,
    graph: &PipelineGraph,
) -> bool {
    if let Some(artifact) = reference
        .strip_prefix("artifact:")
        .or_else(|| reference.strip_prefix("artifact."))
    {
        return artifact_ref_resolves(artifact, context, graph);
    }
    if let Some(checkpoint) = reference
        .strip_prefix("checkpoint:")
        .or_else(|| reference.strip_prefix("checkpoint."))
    {
        return graph_checkpoint_names(graph, context).contains(checkpoint);
    }
    let Some(stem) = runtime_ref_stem(reference) else {
        return false;
    };
    context.runtime_variables.contains(stem) || upstream.contains(stem)
}

fn runtime_ref_stem(reference: &str) -> Option<&str> {
    let stem = reference
        .split(['.', ':'])
        .next()
        .unwrap_or(reference)
        .trim();
    (!stem.is_empty()).then_some(stem)
}

fn is_ref_to_node(reference: &str, node_id: &str) -> bool {
    runtime_ref_stem(reference) == Some(node_id)
}

fn artifact_ref_resolves(
    artifact: &str,
    context: &ValidationContext,
    graph: &PipelineGraph,
) -> bool {
    graph.nodes.contains_key(artifact) || context.artifact_refs.contains(artifact)
}

fn graph_checkpoint_names(graph: &PipelineGraph, context: &ValidationContext) -> BTreeSet<String> {
    let mut names = context.checkpoint_refs.clone();
    for node in graph.nodes.values() {
        names.extend(
            node.checkpoints
                .iter()
                .map(|checkpoint| checkpoint.name.clone()),
        );
    }
    names
}

pub fn detect_cycles_ignoring_marked_back_edges(graph: &PipelineGraph) -> Result<(), String> {
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in &graph.edges {
        if edge_allows_back_edge(edge, graph) {
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
    ) -> Result<(), String> {
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
                    let mut cycle: Vec<&str> = path[cycle_start..].to_vec();
                    cycle.push(next);
                    return Err(format!("cycle detected: {}", cycle.join(" -> ")));
                }
                dfs(next, adj, white, gray, black, path)?;
            }
        }

        path.pop();
        gray.remove(node);
        black.insert(node);
        Ok(())
    }

    let all_nodes: Vec<&str> = graph.nodes.keys().map(String::as_str).collect();
    for node in all_nodes {
        if white.contains(node) {
            dfs(node, &adj, &mut white, &mut gray, &mut black, &mut path)?;
        }
    }
    Ok(())
}

fn edge_allows_back_edge(edge: &PipelineEdge, _graph: &PipelineGraph) -> bool {
    // A back edge must be marked on the EDGE itself (label or condition
    // carrying a retry/back_edge/guard_back marker). codex pre-merge P2: a
    // source node's `max_retries > 0` must NOT suppress cycle detection — it
    // bounds HANDLER retries, not graph traversal, so inferring a back edge
    // from it let real cycles (e.g. `start [max_retries=1] -> b; b -> start`)
    // pass validation and the executor could loop indefinitely. Only the
    // explicit edge marker designates an intentional loop-back.
    let label_marker = edge.label.as_deref().is_some_and(has_back_edge_marker);
    let condition_marker = edge.condition.as_deref().is_some_and(has_back_edge_marker);
    label_marker || condition_marker
}

pub(crate) fn has_back_edge_marker(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    value.contains("retry")
        || value.contains("back_edge")
        || value.contains("back-edge")
        || value.contains("guard_back")
}

fn default_runtime_variables() -> BTreeSet<String> {
    [
        "input",
        "user_input",
        "topic",
        "task",
        // `{label}` is bound alongside `{task}` when a fan-out plan is expanded
        // into per-task workers (executor `sanitize_label_for_filename`), so a
        // worker_prompt referencing it (e.g. a `findings-{label}.md` deliverable)
        // is NOT an unbound template variable.
        "label",
        "context",
        "previous",
        "workspace",
        "cwd",
        "run_dir",
        "artifact",
        "artifacts",
        "outcome",
        "status",
        "files",
        "current_date",
        "now",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn default_known_models() -> BTreeSet<String> {
    [
        "cheap",
        "fast",
        "strong",
        "default",
        "mock",
        "test-model",
        "gpt-4o",
        "gpt-4o-mini",
        "claude-haiku",
        "claude-sonnet-4",
        "claude-sonnet-4-20250514",
        "claude-haiku-4-5-20251001",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn default_known_tools() -> BTreeSet<String> {
    let mut tools: BTreeSet<String> = [
        "shell",
        "exec_command",
        "write_stdin",
        "bash",
        "read_file",
        "write_file",
        "apply_patch",
        "edit_file",
        "diff_edit",
        "glob",
        "grep",
        "list_dir",
        "check_workspace_contract",
        "workspace_log",
        "workspace_show",
        "workspace_diff",
        "view_image",
        "web_search",
        "web_fetch",
        "browser",
        "tool_search",
        "tool_suggest",
        "request_user_input",
        "update_plan",
        "spawn",
        "spawn_agent",
        "send_input",
        "resume_agent",
        "wait_agent",
        "close_agent",
        "delegate",
        "read_task_output",
        "send_file",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    for group in octos_agent::tools::policy::TOOL_GROUPS {
        tools.extend(group.tools.iter().map(|tool| (*tool).to_string()));
    }
    tools
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_dot;

    fn diagnostic_for(diags: &[PipelineDiagnostic], rule_id: RuleId) -> bool {
        diags.iter().any(|d| d.rule_id == rule_id)
    }

    #[test]
    fn rule_23_rejects_shell_handler_and_shell_tools() {
        // The `shell` handler is a rule-23 ERROR.
        let g = parse_dot(
            "digraph s { start [handler=codergen, tools=read_file]; \
             danger [handler=shell, prompt=\"x\"]; start -> danger }",
        )
        .unwrap();
        let diags = diagnostics(&g);
        assert!(
            diags
                .iter()
                .any(|d| d.rule_id == RuleId::NoShell && d.severity == Severity::Error),
            "handler=shell must be a rule-23 Error; got {diags:?}"
        );

        // Shell-family tools (direct), write_stdin (session driver), wildcards,
        // and the runtime group all grant shell → each a rule-23 Error.
        for tools in [
            "read_file,bash",
            "write_stdin",
            "*",
            "exec*",
            "group:runtime",
        ] {
            let g2 = parse_dot(&format!(
                "digraph s {{ a [handler=codergen, tools=\"{tools}\"] }}"
            ))
            .unwrap();
            assert!(
                diagnostics(&g2)
                    .iter()
                    .any(|d| d.rule_id == RuleId::NoShell && d.severity == Severity::Error),
                "tools=\"{tools}\" must be a rule-23 Error (grants shell)"
            );
        }

        // Safe groups + safe prefix wildcards are NOT shell grants.
        for tools in ["group:fs", "read_*", "my_plugin_*", "write_file"] {
            let g3 = parse_dot(&format!(
                "digraph s {{ a [handler=codergen, tools=\"{tools}\"] }}"
            ))
            .unwrap();
            assert!(
                !diagnostic_for(&diagnostics(&g3), RuleId::NoShell),
                "tools=\"{tools}\" does not grant shell and must not be flagged"
            );
        }

        // A safe pipeline is NOT flagged by rule 23.
        let safe = parse_dot(
            "digraph s { a [handler=codergen, tools=read_file]; \
             b [handler=codergen, tools=write_file]; a -> b }",
        )
        .unwrap();
        assert!(
            !diagnostic_for(&diagnostics(&safe), RuleId::NoShell),
            "a non-shell pipeline must not be flagged"
        );
    }

    #[test]
    fn test_valid_graph() {
        let dot = r#"
            digraph test {
                start [prompt="Begin with {input}"]
                finish [prompt="Use {start} and end"]
                start -> finish
            }
        "#;
        let graph = parse_dot(dot).unwrap();
        let diags = diagnostics(&graph);
        assert!(!has_errors(&diags), "unexpected errors: {diags:?}");
        assert!(validate(&graph).is_ok());
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
        let diags = diagnostics(&graph);
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
        let diags = diagnostics(&graph);
        assert!(
            diags.iter().any(|d| d.rule == 2
                && d.message.contains("orphan")
                && d.severity == Severity::Error)
        );
    }

    #[test]
    fn unbound_template_variable_is_error_with_location_and_hint() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [prompt="Summarize {missing}"]
            }
            "#,
        )
        .unwrap();
        let diags = diagnostics(&graph);
        let diag = diags
            .iter()
            .find(|d| d.rule_id == RuleId::TemplateBinding)
            .expect("template binding diagnostic");
        assert_eq!(diag.severity, Severity::Error);
        assert_eq!(diag.location, GraphLocation::Node("start".into()));
        assert!(
            diag.fix_hint
                .as_deref()
                .is_some_and(|hint| hint.contains("bind"))
        );
    }

    #[test]
    fn runtime_variables_bind_template_refs() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [prompt="Summarize {customer_topic}"]
            }
            "#,
        )
        .unwrap();
        let ctx = ValidationContext::default().with_runtime_variables(["customer_topic"]);
        let diags = diagnostics_with_context(&graph, &ctx);
        assert!(!diagnostic_for(&diags, RuleId::TemplateBinding));
    }

    #[test]
    fn dead_edge_is_warning() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [prompt="Begin"]
                finish [prompt="Finish without upstream data"]
                start -> finish
            }
            "#,
        )
        .unwrap();
        let diags = diagnostics(&graph);
        assert!(diags.iter().any(|d| {
            d.rule_id == RuleId::DeadEdge
                && d.severity == Severity::Warning
                && d.location
                    == GraphLocation::Edge {
                        source: "start".into(),
                        target: "finish".into(),
                    }
        }));
    }

    #[test]
    fn consumed_edge_has_no_dead_edge_warning() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [prompt="Begin"]
                finish [prompt="Use {start}"]
                start -> finish
            }
            "#,
        )
        .unwrap();
        let diags = diagnostics(&graph);
        assert!(!diagnostic_for(&diags, RuleId::DeadEdge));
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
        let diags = diagnostics(&graph);
        assert!(diagnostic_for(&diags, RuleId::NoCycle));
    }

    #[test]
    fn max_retries_does_not_hide_a_real_cycle() {
        // codex pre-merge P2: `max_retries` bounds HANDLER retries, not graph
        // traversal. A node with `max_retries > 0` must NOT make its outgoing
        // edges count as back edges — otherwise a genuine cycle slips past
        // validation and the executor can loop indefinitely. Only an explicit
        // edge marker (label/condition) designates an intentional loop-back.
        let graph = parse_dot(
            r#"
            digraph test {
                start [prompt="A", max_retries="1"]
                b [prompt="B"]
                start -> b
                b -> start
            }
            "#,
        )
        .unwrap();
        let diags = diagnostics(&graph);
        assert!(
            diagnostic_for(&diags, RuleId::NoCycle),
            "max_retries on the cycle source must not suppress NoCycle"
        );
    }

    #[test]
    fn retry_marked_back_edge_is_allowed() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [prompt="A"]
                b [prompt="B"]
                start -> b
                b -> start [condition="retry"]
            }
            "#,
        )
        .unwrap();
        let diags = diagnostics(&graph);
        assert!(!diagnostic_for(&diags, RuleId::NoCycle));
    }

    #[test]
    fn unknown_model_is_error() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [prompt="A", model="not-in-catalog"]
            }
            "#,
        )
        .unwrap();
        let diags = diagnostics(&graph);
        assert!(diagnostic_for(&diags, RuleId::KnownModel));
    }

    #[test]
    fn known_model_from_context_is_allowed() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [prompt="A", model="custom-fast"]
            }
            "#,
        )
        .unwrap();
        let ctx = ValidationContext::default().with_known_models(["custom-fast"]);
        let diags = diagnostics_with_context(&graph, &ctx);
        assert!(!diagnostic_for(&diags, RuleId::KnownModel));
    }

    #[test]
    fn unknown_tool_or_group_is_error() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [prompt="A", tools="read_file,no_such_tool,group:nope"]
            }
            "#,
        )
        .unwrap();
        let diags = diagnostics(&graph);
        assert_eq!(
            diags
                .iter()
                .filter(|d| d.rule_id == RuleId::KnownToolPolicy)
                .count(),
            2
        );
    }

    #[test]
    fn explicit_empty_tools_deny_all_passes_validation() {
        // codex pre-merge P2 (round-2): `tools=""` is the deny-all sentinel
        // (parser keeps a single `""` entry). Rule 19 must NOT flag that empty
        // marker as an unknown tool — otherwise the restored deny-all syntax
        // fails validation before the handler interprets it.
        let graph = parse_dot(
            r#"
            digraph test {
                start [prompt="text only, no tools", tools=""]
            }
            "#,
        )
        .unwrap();
        // Sanity: the parser kept the deny-all marker.
        assert_eq!(graph.nodes["start"].tools, vec![String::new()]);
        let diags = diagnostics(&graph);
        assert!(
            !diagnostic_for(&diags, RuleId::KnownToolPolicy),
            "tools=\"\" deny-all must not trip Rule 19",
        );
    }

    #[test]
    fn known_tools_helper_skips_plugin_load_when_builtins_cover_refs() {
        // No plugin_dirs -> just built-ins, regardless of references.
        let wd = std::path::Path::new(".");
        let names = known_tool_names_with_plugins(wd, &[], false, &["read_file".into()]);
        assert!(names.iter().any(|n| n == "read_file"));

        // A wildcard already covered by a built-in (`read_*` matches
        // `read_file`) must NOT require plugin discovery — with a nonexistent
        // plugin dir the load would no-op, so we just assert built-ins return
        // and the call doesn't error. (codex round-7 P2: wildcards are only
        // "covered" when a built-in matches the prefix.)
        let missing = std::path::PathBuf::from("/nonexistent/plugins");
        let names2 = known_tool_names_with_plugins(wd, &[missing], false, &["read_*".into()]);
        assert!(names2.iter().any(|n| n == "read_file"));

        // A plugin-prefix wildcard with no matching built-in flags
        // needs_plugins (the load is attempted; nonexistent dir -> built-ins).
        let missing2 = std::path::PathBuf::from("/nonexistent/plugins");
        let names3 = known_tool_names_with_plugins(wd, &[missing2], false, &["my_plugin_*".into()]);
        assert!(names3.iter().any(|n| n == "read_file"));
    }

    #[test]
    fn fanout_over_cap_is_error() {
        let mut dot = String::from("digraph test { start [prompt=\"A\"]\n");
        for i in 0..=MAX_DECLARED_FANOUT_DEGREE {
            dot.push_str(&format!("n{i} [prompt=\"Use {{start}}\"]\nstart -> n{i}\n"));
        }
        dot.push('}');
        let graph = parse_dot(&dot).unwrap();
        let diags = diagnostics(&graph);
        assert!(diagnostic_for(&diags, RuleId::FanoutBound));
    }

    #[test]
    fn dynamic_parallel_max_tasks_over_cap_is_error() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [handler="dynamic_parallel", prompt="Plan", converge="merge", max_tasks="51"]
                merge [prompt="Merge"]
                start -> merge
            }
            "#,
        )
        .unwrap();
        let diags = diagnostics(&graph);
        assert!(diagnostic_for(&diags, RuleId::FanoutBound));
    }

    #[test]
    fn human_gate_requires_resolver() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [handler="gate", human_gate="true", prompt="Approve?"]
            }
            "#,
        )
        .unwrap();
        let diags = diagnostics(&graph);
        assert!(diagnostic_for(&diags, RuleId::HumanGateResolver));
    }

    #[test]
    fn human_gate_with_resolver_is_allowed() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [handler="gate", human_gate="true", resolver="operator", prompt="Approve?"]
            }
            "#,
        )
        .unwrap();
        let diags = diagnostics(&graph);
        assert!(!diagnostic_for(&diags, RuleId::HumanGateResolver));
    }

    #[test]
    fn dangling_artifact_and_checkpoint_refs_are_errors() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [prompt="A", requires_artifact="missing", requires_checkpoint="nope"]
            }
            "#,
        )
        .unwrap();
        let diags = diagnostics(&graph);
        assert_eq!(
            diags
                .iter()
                .filter(|d| d.rule_id == RuleId::ReferenceResolution)
                .count(),
            2
        );
    }

    #[test]
    fn artifact_and_checkpoint_refs_can_resolve() {
        let graph = parse_dot(
            r#"
            digraph test {
                start [prompt="A", checkpoint="post_start"]
                finish [prompt="Use {artifact:start} and {checkpoint:post_start}", requires_artifact="start", requires_checkpoint="post_start"]
                start -> finish
            }
            "#,
        )
        .unwrap();
        let diags = diagnostics(&graph);
        assert!(!diagnostic_for(&diags, RuleId::ReferenceResolution));
        assert!(!diagnostic_for(&diags, RuleId::TemplateBinding));
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
        let diags = diagnostics(&graph);
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
                start [handler="parallel", converge="nonexistent"]
                a [prompt="A"]
                start -> a
            }
        "#;
        let graph = parse_dot(dot).unwrap();
        let diags = diagnostics(&graph);
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
                start [handler="parallel", converge="merge"]
                a [prompt="Use {start}"]
                b [prompt="Use {start}"]
                merge [prompt="Use {a} and {b}"]
                start -> a
                start -> b
                a -> merge
                b -> merge
            }
        "#;
        let graph = parse_dot(dot).unwrap();
        let diags = diagnostics(&graph);
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
        let diags = diagnostics(&graph);
        assert!(has_errors(&diags));
        assert!(diags.iter().any(|d| d.rule == 10));
    }

    #[test]
    fn test_dynamic_parallel_missing_converge() {
        let dot = r#"
            digraph test {
                start [handler="dynamic_parallel", prompt="Plan"]
                next [prompt="Use {start}"]
                start -> next
            }
        "#;
        let graph = parse_dot(dot).unwrap();
        let diags = diagnostics(&graph);
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
                start [handler="dynamic_parallel", converge="nonexistent", prompt="Plan"]
                next [prompt="Use {start}"]
                start -> next
            }
        "#;
        let graph = parse_dot(dot).unwrap();
        let diags = diagnostics(&graph);
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
                start [handler="dynamic_parallel", converge="analyze"]
                analyze [prompt="Use {start}"]
                start -> analyze
            }
        "#;
        let graph = parse_dot(dot).unwrap();
        let diags = diagnostics(&graph);
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
                start [handler="dynamic_parallel", converge="analyze", prompt="Generate angles"]
                analyze [prompt="Use {start}"]
                synthesize [prompt="Use {analyze}", goal_gate="true"]
                start -> analyze
                analyze -> synthesize
            }
        "#;
        let graph = parse_dot(dot).unwrap();
        let diags = diagnostics(&graph);
        assert!(!has_errors(&diags), "unexpected errors: {diags:?}");
        assert!(!diags.iter().any(|d| d.rule == 14));
    }
}

#[cfg(test)]
mod resolvable_models_tests {
    use super::*;

    /// The validator must certify RESOLVABILITY, not mere presence in the
    /// catalog (#1901, layer 5).
    ///
    /// `known_models` was fed from the model catalog, which lists every model
    /// the profile could theoretically use regardless of what the provider
    /// router registered. So a graph naming a catalogued-but-unregistered model
    /// PASSED pre-flight and then failed at runtime — the check certified a
    /// graph that cannot run. After #1902 it stopped crashing and silently ran
    /// on the default provider instead, which is worse: the validator said yes,
    /// the run looked healthy, and the requested model was never used.
    #[test]
    fn catalogued_but_unregistered_models_are_rejected() {
        // The catalog knows it; the router does not register it.
        let ctx = ValidationContext::default()
            .with_known_models(["qwen3-max".to_string(), "strong".to_string()])
            .with_resolvable_models(["cheap".to_string(), "strong".to_string()]);

        assert!(
            !ctx.known_models.contains("qwen3-max"),
            "a catalogued-but-unregistered model must NOT count as known once \
             the router is the authority"
        );
        assert!(
            ctx.known_models.contains("strong"),
            "a registered lane key must stay known"
        );
    }

    /// With no router (or an empty one) the catalog set is kept. Validation must
    /// not become STRICTER than reality when it has no way to know what is
    /// registered — that would reject working graphs.
    #[test]
    fn an_absent_router_leaves_the_catalog_set_intact() {
        let ctx = ValidationContext::default()
            .with_known_models(["qwen3-max".to_string()])
            .with_resolvable_models(Vec::<String>::new());

        assert!(
            ctx.known_models.contains("qwen3-max"),
            "with no router keys the catalog set must be preserved, not emptied"
        );
    }

    /// The rejection must not send the reader to `model_catalog.json` — the
    /// model is usually IN the catalog and merely unregistered, so that advice
    /// points at the wrong file (#1901, layer 4).
    #[test]
    fn rejection_names_the_router_not_the_catalog() {
        let ctx = ValidationContext::default().with_resolvable_models(["strong".to_string()]);
        let mut diags = Vec::new();
        validate_model_name("qwen3-max", &ctx, GraphLocation::Graph, &mut diags);

        assert_eq!(diags.len(), 1, "an unresolvable model must be diagnosed");
        let hint = diags[0].fix_hint.clone().unwrap_or_default();
        assert!(
            !hint.contains("model_catalog.json"),
            "the hint must not point at the catalog, where the model IS present: {hint}"
        );
        assert!(
            hint.contains("provider router") && hint.contains("strong"),
            "the hint must name the router and list what IS resolvable: {hint}"
        );
    }
}
