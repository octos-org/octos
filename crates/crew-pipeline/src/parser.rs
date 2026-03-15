//! Hand-written DOT parser for the pipeline subset.
//!
//! Supports:
//! - `digraph name { ... }`
//! - `graph [key=value, ...]`  (graph-level attributes)
//! - `node_id [key=value, ...]`  (node declarations)
//! - `node_a -> node_b [key=value, ...]`  (edge declarations)
//! - `//` and `/* */` comments
//! - Quoted strings with escape sequences

use std::collections::HashMap;

use eyre::{Result, WrapErr};

use crate::graph::{HandlerKind, PipelineEdge, PipelineGraph, PipelineNode, Subgraph};

/// Parse a DOT string into a `PipelineGraph`.
pub fn parse_dot(input: &str) -> Result<PipelineGraph> {
    let mut parser = DotParser::new(input);
    parser.parse()
}

/// Internal parser state.
struct DotParser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> DotParser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn parse(&mut self) -> Result<PipelineGraph> {
        self.skip_ws();

        // Parse "digraph" keyword
        self.expect_keyword("digraph")
            .wrap_err("expected 'digraph' keyword")?;
        self.skip_ws();

        // Parse graph name
        let id = self.parse_identifier().wrap_err("expected graph name")?;
        self.skip_ws();

        // Parse opening brace
        self.expect_char('{').wrap_err("expected '{'")?;

        let mut graph = PipelineGraph {
            id,
            label: None,
            default_model: None,
            nodes: HashMap::new(),
            edges: Vec::new(),
            subgraphs: Vec::new(),
        };

        // Parse statements
        loop {
            self.skip_ws();
            if self.peek() == Some('}') {
                self.advance();
                break;
            }
            if self.is_eof() {
                eyre::bail!("unexpected EOF, expected '}}'");
            }
            self.parse_statement(&mut graph)?;
        }

        // Ensure all nodes referenced in edges exist (auto-create with defaults)
        for edge in &graph.edges {
            if !graph.nodes.contains_key(&edge.source) {
                graph.nodes.insert(
                    edge.source.clone(),
                    PipelineNode {
                        id: edge.source.clone(),
                        ..Default::default()
                    },
                );
            }
            if !graph.nodes.contains_key(&edge.target) {
                graph.nodes.insert(
                    edge.target.clone(),
                    PipelineNode {
                        id: edge.target.clone(),
                        ..Default::default()
                    },
                );
            }
        }

        Ok(graph)
    }

    fn parse_statement(&mut self, graph: &mut PipelineGraph) -> Result<()> {
        self.skip_ws();

        // Check for graph-level attributes: `graph [...]`
        if self.try_keyword("graph") {
            self.skip_ws();
            if self.peek() == Some('[') {
                let attrs = self.parse_attributes()?;
                apply_graph_attrs(graph, &attrs);
            }
            self.skip_optional_semicolon();
            return Ok(());
        }

        // Check for subgraph: `subgraph name { ... }`
        if self.try_keyword("subgraph") {
            self.parse_subgraph(graph)?;
            self.skip_optional_semicolon();
            return Ok(());
        }

        // Parse an identifier (could be node or start of edge)
        let first_id = self
            .parse_identifier()
            .wrap_err("expected node ID or '}'")?;
        self.skip_ws();

        // Check for edge: `->` means this is an edge
        if self.try_str("->") {
            let _ = self.parse_edge_chain(graph, first_id)?;
        } else {
            // Node declaration
            let attrs = if self.peek() == Some('[') {
                self.parse_attributes()?
            } else {
                HashMap::new()
            };
            let node = build_node(&first_id, &attrs);
            graph.nodes.insert(first_id, node);
        }

        self.skip_optional_semicolon();
        Ok(())
    }

    /// Parse a subgraph block: `subgraph name { ... }`.
    /// Collects node/edge declarations inside the block and tags nodes
    /// as belonging to the subgraph.
    fn parse_subgraph(&mut self, graph: &mut PipelineGraph) -> Result<()> {
        self.skip_ws();
        let subgraph_id = self.parse_identifier().wrap_err("expected subgraph name")?;
        self.skip_ws();
        self.expect_char('{')
            .wrap_err("expected '{' after subgraph name")?;

        let mut label = None;
        let mut node_ids = Vec::new();

        // Parse statements inside the subgraph
        loop {
            self.skip_ws();
            if self.peek() == Some('}') {
                self.advance();
                break;
            }
            if self.is_eof() {
                eyre::bail!("unexpected EOF in subgraph '{}'", subgraph_id);
            }

            // Handle graph-level attrs inside subgraph (e.g. label)
            if self.try_keyword("graph") {
                self.skip_ws();
                if self.peek() == Some('[') {
                    let attrs = self.parse_attributes()?;
                    if let Some(l) = attrs.get("label") {
                        label = Some(l.clone());
                    }
                }
                self.skip_optional_semicolon();
                continue;
            }

            // Parse identifier — could be node or edge
            let first_id = self
                .parse_identifier()
                .wrap_err("expected node ID in subgraph")?;
            self.skip_ws();

            if self.try_str("->") {
                // Edge inside subgraph — add to main graph, track all chain nodes
                let chain = self.parse_edge_chain(graph, first_id)?;
                for id in chain {
                    if !node_ids.contains(&id) {
                        node_ids.push(id);
                    }
                }
            } else {
                // Node declaration
                let attrs = if self.peek() == Some('[') {
                    self.parse_attributes()?
                } else {
                    HashMap::new()
                };
                let node = build_node(&first_id, &attrs);
                graph.nodes.insert(first_id.clone(), node);
                if !node_ids.contains(&first_id) {
                    node_ids.push(first_id);
                }
            }

            self.skip_optional_semicolon();
        }

        graph.subgraphs.push(Subgraph {
            id: subgraph_id,
            label,
            node_ids,
        });

        Ok(())
    }

    /// Parse an edge chain: `a -> b -> c [attrs]`.
    /// Returns all node IDs in the chain.
    fn parse_edge_chain(
        &mut self,
        graph: &mut PipelineGraph,
        first: String,
    ) -> Result<Vec<String>> {
        let mut chain = vec![first];

        loop {
            self.skip_ws();
            let next = self
                .parse_identifier()
                .wrap_err("expected node ID after '->'")?;
            chain.push(next);
            self.skip_ws();
            if !self.try_str("->") {
                break;
            }
        }

        // Optional attributes apply to all edges in the chain
        self.skip_ws();
        let attrs = if self.peek() == Some('[') {
            self.parse_attributes()?
        } else {
            HashMap::new()
        };

        for pair in chain.windows(2) {
            let edge = build_edge(&pair[0], &pair[1], &attrs);
            graph.edges.push(edge);
        }

        Ok(chain)
    }

    /// Parse `[key=value, key=value, ...]` or `[key="value", ...]`.
    fn parse_attributes(&mut self) -> Result<HashMap<String, String>> {
        self.expect_char('[')?;
        let mut attrs = HashMap::new();

        loop {
            self.skip_ws();
            if self.peek() == Some(']') {
                self.advance();
                break;
            }
            if self.is_eof() {
                eyre::bail!("unexpected EOF in attribute list");
            }

            // Skip commas and semicolons between attributes
            if self.peek() == Some(',') || self.peek() == Some(';') {
                self.advance();
                continue;
            }

            let key = self
                .parse_identifier()
                .wrap_err("expected attribute name")?;
            self.skip_ws();
            self.expect_char('=')
                .wrap_err("expected '=' in attribute")?;
            self.skip_ws();
            let value = self.parse_value()?;
            attrs.insert(key, value);
        }

        Ok(attrs)
    }

    /// Parse a value: quoted string or bare identifier/number.
    fn parse_value(&mut self) -> Result<String> {
        if self.peek() == Some('"') {
            self.parse_quoted_string()
        } else {
            self.parse_identifier()
        }
    }

    /// Parse a quoted string with escape handling.
    fn parse_quoted_string(&mut self) -> Result<String> {
        self.expect_char('"')?;
        let mut result = String::new();

        loop {
            match self.next_char() {
                Some('"') => break,
                Some('\\') => match self.next_char() {
                    Some('n') => result.push('\n'),
                    Some('t') => result.push('\t'),
                    Some('\\') => result.push('\\'),
                    Some('"') => result.push('"'),
                    Some(c) => {
                        result.push('\\');
                        result.push(c);
                    }
                    None => eyre::bail!("unexpected EOF in string escape"),
                },
                Some(c) => result.push(c),
                None => eyre::bail!("unexpected EOF in quoted string"),
            }
        }

        Ok(result)
    }

    /// Parse a bare identifier (alphanumeric + underscore + hyphen + dot).
    fn parse_identifier(&mut self) -> Result<String> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_alphanumeric() || c == '_' || c == '-' || c == '.' {
                self.advance();
            } else {
                break;
            }
        }
        if self.pos == start {
            eyre::bail!("expected identifier at position {}", self.pos);
        }
        Ok(self.input[start..self.pos].to_string())
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<()> {
        let start = self.pos;
        if let Ok(id) = self.parse_identifier() {
            if id == kw {
                return Ok(());
            }
        }
        self.pos = start;
        eyre::bail!("expected keyword '{kw}'")
    }

    fn try_keyword(&mut self, kw: &str) -> bool {
        let start = self.pos;
        if let Ok(id) = self.parse_identifier() {
            if id == kw {
                return true;
            }
        }
        self.pos = start;
        false
    }

    fn try_str(&mut self, s: &str) -> bool {
        if self.input[self.pos..].starts_with(s) {
            self.pos += s.len();
            true
        } else {
            false
        }
    }

    fn expect_char(&mut self, c: char) -> Result<()> {
        match self.peek() {
            Some(ch) if ch == c => {
                self.advance();
                Ok(())
            }
            Some(ch) => eyre::bail!("expected '{}', found '{}'", c, ch),
            None => eyre::bail!("expected '{}', found EOF", c),
        }
    }

    fn skip_optional_semicolon(&mut self) {
        self.skip_ws();
        if self.peek() == Some(';') {
            self.advance();
        }
    }

    fn peek(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    fn next_char(&mut self) -> Option<char> {
        let c = self.input[self.pos..].chars().next()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn advance(&mut self) {
        if let Some(c) = self.input[self.pos..].chars().next() {
            self.pos += c.len_utf8();
        }
    }

    fn is_eof(&self) -> bool {
        self.pos >= self.input.len()
    }

    /// Skip whitespace and comments.
    fn skip_ws(&mut self) {
        loop {
            // Skip whitespace
            while let Some(c) = self.peek() {
                if c.is_whitespace() {
                    self.advance();
                } else {
                    break;
                }
            }

            // Skip line comments
            if self.input[self.pos..].starts_with("//") {
                while let Some(c) = self.peek() {
                    self.advance();
                    if c == '\n' {
                        break;
                    }
                }
                continue;
            }

            // Skip block comments
            if self.input[self.pos..].starts_with("/*") {
                self.pos += 2;
                while !self.is_eof() {
                    if self.input[self.pos..].starts_with("*/") {
                        self.pos += 2;
                        break;
                    }
                    self.advance();
                }
                continue;
            }

            break;
        }
    }
}

fn apply_graph_attrs(graph: &mut PipelineGraph, attrs: &HashMap<String, String>) {
    if let Some(label) = attrs.get("label") {
        graph.label = Some(label.clone());
    }
    if let Some(model) = attrs.get("default_model") {
        graph.default_model = Some(model.clone());
    }
}

/// Parse a duration string like "900s", "15m", "2h" into seconds.
/// Falls back to plain integer parsing (interpreted as seconds).
/// Returns `None` on overflow or unrecognized format.
fn parse_duration_secs(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix('s') {
        n.trim().parse::<u64>().ok()
    } else if let Some(n) = s.strip_suffix('m') {
        n.trim().parse::<u64>().ok().and_then(|v| v.checked_mul(60))
    } else if let Some(n) = s.strip_suffix('h') {
        n.trim()
            .parse::<u64>()
            .ok()
            .and_then(|v| v.checked_mul(3600))
    } else {
        s.parse::<u64>().ok()
    }
}

/// Parse a boolean string ("true", "false", "yes", "no", "1", "0").
fn parse_bool(s: &str) -> Option<bool> {
    match s.trim() {
        "true" | "yes" | "1" => Some(true),
        "false" | "no" | "0" => Some(false),
        _ => None,
    }
}

fn build_node(id: &str, attrs: &HashMap<String, String>) -> PipelineNode {
    // Resolution: explicit handler > shape-based > default (codergen)
    let handler = attrs
        .get("handler")
        .and_then(|s| HandlerKind::from_str(s))
        .or_else(|| attrs.get("shape").and_then(|s| HandlerKind::from_shape(s)))
        .unwrap_or(HandlerKind::Codergen);

    let tools = attrs
        .get("tools")
        .map(|s| s.split(',').map(|t| t.trim().to_string()).collect())
        .unwrap_or_default();

    PipelineNode {
        id: id.to_string(),
        handler,
        prompt: attrs.get("prompt").cloned(),
        label: attrs.get("label").cloned(),
        model: attrs.get("model").cloned(),
        context_window: attrs.get("context_window").and_then(|s| s.parse().ok()),
        max_output_tokens: attrs.get("max_output_tokens").and_then(|s| s.parse().ok()),
        tools,
        goal_gate: attrs
            .get("goal_gate")
            .and_then(|s| parse_bool(s))
            .unwrap_or(false),
        max_retries: attrs
            .get("max_retries")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        timeout_secs: attrs
            .get("timeout_secs")
            .and_then(|s| parse_duration_secs(s)),
        suggested_next: attrs.get("suggested_next").cloned(),
        converge: attrs.get("converge").cloned(),
        worker_prompt: attrs.get("worker_prompt").cloned(),
        planner_model: attrs.get("planner_model").cloned(),
        max_tasks: attrs.get("max_tasks").and_then(|s| s.parse().ok()),
    }
}

fn build_edge(source: &str, target: &str, attrs: &HashMap<String, String>) -> PipelineEdge {
    PipelineEdge {
        source: source.to_string(),
        target: target.to_string(),
        label: attrs.get("label").cloned(),
        condition: attrs.get("condition").cloned(),
        weight: attrs
            .get("weight")
            .and_then(|s| s.parse().ok())
            .unwrap_or(1.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_graph() {
        let dot = r#"
            digraph test {
                start [prompt="Begin here", handler="codergen"]
                finish [prompt="Wrap up"]
                start -> finish
            }
        "#;

        let graph = parse_dot(dot).unwrap();
        assert_eq!(graph.id, "test");
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.edges.len(), 1);
        assert_eq!(graph.edges[0].source, "start");
        assert_eq!(graph.edges[0].target, "finish");
    }

    #[test]
    fn test_parse_graph_attributes() {
        let dot = r#"
            digraph research {
                graph [label="Deep Research", default_model="cheap"]
                search [prompt="Search the web", model="cheap"]
                analyze [prompt="Analyze results", model="strong"]
                search -> analyze
            }
        "#;

        let graph = parse_dot(dot).unwrap();
        assert_eq!(graph.label.as_deref(), Some("Deep Research"));
        assert_eq!(graph.default_model.as_deref(), Some("cheap"));
        assert_eq!(graph.nodes["search"].model.as_deref(), Some("cheap"));
        assert_eq!(graph.nodes["analyze"].model.as_deref(), Some("strong"));
    }

    #[test]
    fn test_parse_edge_attributes() {
        let dot = r#"
            digraph test {
                a -> b [condition="outcome.status == \"pass\"", weight="2.0"]
                a -> c [condition="outcome.status == \"fail\""]
            }
        "#;

        let graph = parse_dot(dot).unwrap();
        assert_eq!(graph.edges.len(), 2);
        assert!(graph.edges[0].condition.is_some());
        assert_eq!(graph.edges[0].weight, 2.0);
        assert_eq!(graph.edges[1].weight, 1.0);
    }

    #[test]
    fn test_parse_edge_chain() {
        let dot = r#"
            digraph test {
                a -> b -> c -> d
            }
        "#;

        let graph = parse_dot(dot).unwrap();
        assert_eq!(graph.edges.len(), 3);
        assert_eq!(graph.edges[0].source, "a");
        assert_eq!(graph.edges[0].target, "b");
        assert_eq!(graph.edges[1].source, "b");
        assert_eq!(graph.edges[1].target, "c");
        assert_eq!(graph.edges[2].source, "c");
        assert_eq!(graph.edges[2].target, "d");
    }

    #[test]
    fn test_parse_comments() {
        let dot = r#"
            // This is a comment
            digraph test {
                /* Block comment */
                a [prompt="hello"]
                // Another comment
                a -> b
            }
        "#;

        let graph = parse_dot(dot).unwrap();
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.edges.len(), 1);
    }

    #[test]
    fn test_parse_node_tools() {
        let dot = r#"
            digraph test {
                search [prompt="Find stuff", tools="web_search,web_fetch"]
            }
        "#;

        let graph = parse_dot(dot).unwrap();
        let node = &graph.nodes["search"];
        assert_eq!(node.tools, vec!["web_search", "web_fetch"]);
    }

    #[test]
    fn test_auto_create_nodes_from_edges() {
        let dot = r#"
            digraph test {
                a -> b -> c
            }
        "#;

        let graph = parse_dot(dot).unwrap();
        assert!(graph.nodes.contains_key("a"));
        assert!(graph.nodes.contains_key("b"));
        assert!(graph.nodes.contains_key("c"));
    }

    #[test]
    fn test_parse_parallel_converge() {
        let dot = r#"
            digraph test {
                fan [handler="parallel", converge="merge"]
                a [prompt="A"]
                b [prompt="B"]
                merge [prompt="Merge results"]
                fan -> a
                fan -> b
                a -> merge
                b -> merge
            }
        "#;

        let graph = parse_dot(dot).unwrap();
        let fan = &graph.nodes["fan"];
        assert_eq!(fan.handler, HandlerKind::Parallel);
        assert_eq!(fan.converge.as_deref(), Some("merge"));
        assert_eq!(graph.edges.len(), 4);
    }

    #[test]
    fn test_parse_goal_gate() {
        let dot = r#"
            digraph test {
                review [prompt="Review code", goal_gate="true"]
            }
        "#;

        let graph = parse_dot(dot).unwrap();
        assert!(graph.nodes["review"].goal_gate);
    }

    #[test]
    fn test_parse_dynamic_parallel() {
        let dot = r#"
            digraph test {
                plan [
                    handler="dynamic_parallel",
                    converge="analyze",
                    prompt="Plan search angles",
                    worker_prompt="You are a specialist.\n\n{task}",
                    planner_model="strong",
                    max_tasks="6",
                    tools="web_search,read_file",
                    model="cheap",
                    timeout_secs="300"
                ]
                analyze [prompt="Analyze results", model="strong"]
                plan -> analyze
            }
        "#;

        let graph = parse_dot(dot).unwrap();
        let plan = &graph.nodes["plan"];
        assert_eq!(plan.handler, HandlerKind::DynamicParallel);
        assert_eq!(plan.converge.as_deref(), Some("analyze"));
        assert_eq!(
            plan.worker_prompt.as_deref(),
            Some("You are a specialist.\n\n{task}")
        );
        assert_eq!(plan.planner_model.as_deref(), Some("strong"));
        assert_eq!(plan.max_tasks, Some(6));
        assert_eq!(plan.model.as_deref(), Some("cheap"));
        assert_eq!(plan.tools, vec!["web_search", "read_file"]);
        assert_eq!(plan.timeout_secs, Some(300));
    }

    #[test]
    fn test_parse_duration_secs() {
        assert_eq!(parse_duration_secs("300"), Some(300));
        assert_eq!(parse_duration_secs("900s"), Some(900));
        assert_eq!(parse_duration_secs("15m"), Some(900));
        assert_eq!(parse_duration_secs("2h"), Some(7200));
        assert_eq!(parse_duration_secs("bad"), None);
    }

    #[test]
    fn test_parse_duration_overflow() {
        // Huge values should return None via checked_mul, not wrap
        assert_eq!(parse_duration_secs("9999999999999999999h"), None);
        assert_eq!(parse_duration_secs("9999999999999999999m"), None);
    }

    #[test]
    fn test_parse_bool_values() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool("yes"), Some(true));
        assert_eq!(parse_bool("1"), Some(true));
        assert_eq!(parse_bool("false"), Some(false));
        assert_eq!(parse_bool("no"), Some(false));
        assert_eq!(parse_bool("0"), Some(false));
        assert_eq!(parse_bool("maybe"), None);
    }

    #[test]
    fn test_typed_duration_in_node() {
        let dot = r#"
            digraph test {
                task [prompt="Do work", timeout_secs="15m"]
            }
        "#;
        let graph = parse_dot(dot).unwrap();
        assert_eq!(graph.nodes["task"].timeout_secs, Some(900));
    }

    #[test]
    fn test_typed_bool_in_node() {
        let dot = r#"
            digraph test {
                gate [prompt="Check", goal_gate="yes"]
            }
        "#;
        let graph = parse_dot(dot).unwrap();
        assert!(graph.nodes["gate"].goal_gate);

        let dot2 = r#"
            digraph test {
                gate [prompt="Check", goal_gate="no"]
            }
        "#;
        let graph2 = parse_dot(dot2).unwrap();
        assert!(!graph2.nodes["gate"].goal_gate);
    }

    #[test]
    fn test_parse_dynamic_parallel_defaults() {
        let dot = r#"
            digraph test {
                plan [handler="dynamic_parallel", converge="next"]
                next [prompt="Next step"]
                plan -> next
            }
        "#;

        let graph = parse_dot(dot).unwrap();
        let plan = &graph.nodes["plan"];
        assert_eq!(plan.handler, HandlerKind::DynamicParallel);
        assert!(plan.worker_prompt.is_none());
        assert!(plan.planner_model.is_none());
        assert!(plan.max_tasks.is_none());
    }

    #[test]
    fn test_parse_subgraph() {
        let dot = r#"
            digraph test {
                start [prompt="Begin"]

                subgraph cluster_research {
                    graph [label="Research Phase"]
                    search [prompt="Search"]
                    analyze [prompt="Analyze"]
                    search -> analyze
                }

                start -> search
                analyze -> finish
                finish [prompt="Done"]
            }
        "#;

        let graph = parse_dot(dot).unwrap();
        assert_eq!(graph.subgraphs.len(), 1);
        assert_eq!(graph.subgraphs[0].id, "cluster_research");
        assert_eq!(graph.subgraphs[0].label.as_deref(), Some("Research Phase"));
        assert!(graph.subgraphs[0].node_ids.contains(&"search".to_string()));
        assert!(graph.subgraphs[0].node_ids.contains(&"analyze".to_string()));
        // Nodes should be in the main graph too
        assert!(graph.nodes.contains_key("search"));
        assert!(graph.nodes.contains_key("analyze"));
    }

    #[test]
    fn test_parse_multiple_subgraphs() {
        let dot = r#"
            digraph test {
                subgraph phase1 {
                    a [prompt="A"]
                    b [prompt="B"]
                }
                subgraph phase2 {
                    c [prompt="C"]
                }
                a -> c
            }
        "#;

        let graph = parse_dot(dot).unwrap();
        assert_eq!(graph.subgraphs.len(), 2);
        assert_eq!(graph.subgraphs[0].id, "phase1");
        assert_eq!(graph.subgraphs[0].node_ids.len(), 2);
        assert_eq!(graph.subgraphs[1].id, "phase2");
        assert_eq!(graph.subgraphs[1].node_ids.len(), 1);
    }

    #[test]
    fn test_subgraph_edge_only_tracks_all_nodes() {
        let dot = r#"
            digraph test {
                subgraph cluster_flow {
                    a -> b -> c
                }
            }
        "#;

        let graph = parse_dot(dot).unwrap();
        let sg = &graph.subgraphs[0];
        assert_eq!(sg.node_ids.len(), 3);
        assert!(sg.node_ids.contains(&"a".to_string()));
        assert!(sg.node_ids.contains(&"b".to_string()));
        assert!(sg.node_ids.contains(&"c".to_string()));
    }

    #[test]
    fn test_no_subgraphs_by_default() {
        let dot = r#"
            digraph test {
                a -> b
            }
        "#;
        let graph = parse_dot(dot).unwrap();
        assert!(graph.subgraphs.is_empty());
    }
}
