//! Pipeline DOT graph guard — a before_tool_call hook for `run_pipeline`.
//!
//! Protocol:
//!   - Reads hook payload JSON from stdin (contains tool_name + tool_input)
//!   - Validates the DOT graph structure
//!   - Fixes model selection, tools, and max_output_tokens deterministically
//!   - Exit 0 = allow (DOT is fine)
//!   - Exit 2 = modified (stdout = corrected tool args JSON)
//!   - Exit 1 = deny (stdout = reason)
//!
//! Validation checks:
//!   - DOT parses as valid digraph
//!   - All referenced models exist in provider list
//!   - Graph has at least one node and is connected
//!   - No cycles that would cause infinite execution
//!
//! Deterministic fixes:
//!   1. Model selection based on node role + QoS scores
//!   2. Tools: analyze gets read_file,list_dir,glob; synthesize gets read_file,write_file
//!   3. max_output_tokens: set from provider capabilities if missing

use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::io::Read;

#[derive(Deserialize)]
struct HookPayload {
    tool_name: Option<String>,
    tool_input: Option<serde_json::Value>,
}

#[derive(Deserialize, Clone)]
struct ModelMeta {
    key: String,
    max_output: u32,
    context: u32,
    #[serde(default)]
    qos: f64,
}

// ── Lightweight DOT validation ──────────────────────────────────

struct DotValidation {
    nodes: Vec<String>,
    edges: Vec<(String, String)>,
    errors: Vec<String>,
}

fn validate_dot(dot: &str) -> DotValidation {
    let mut v = DotValidation {
        nodes: Vec::new(),
        edges: Vec::new(),
        errors: Vec::new(),
    };

    // Check basic structure
    let trimmed = dot.trim();
    if !trimmed.starts_with("digraph") {
        v.errors.push("DOT must start with 'digraph'".into());
        return v;
    }

    // Find matching braces
    let open = match trimmed.find('{') {
        Some(p) => p,
        None => {
            v.errors.push("missing opening brace '{'".into());
            return v;
        }
    };
    let close = match trimmed.rfind('}') {
        Some(p) => p,
        None => {
            v.errors.push("missing closing brace '}'".into());
            return v;
        }
    };
    if close <= open {
        v.errors.push("malformed braces".into());
        return v;
    }

    let body = &trimmed[open + 1..close];

    // Parse nodes and edges from body
    // Split by newlines and semicolons
    for raw_line in body.split('\n') {
        let line = raw_line.trim().trim_end_matches(';').trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }

        // Edge: "A -> B"
        if line.contains("->") {
            let parts: Vec<&str> = line.split("->").collect();
            if parts.len() == 2 {
                let src = parts[0].trim().trim_matches('"').to_string();
                let dst = parts[1].trim().split('[').next().unwrap_or("").trim().trim_matches('"').to_string();
                if !src.is_empty() && !dst.is_empty() {
                    v.edges.push((src, dst));
                }
            }
            continue;
        }

        // Node: "name [attrs...]" or "name[attrs...]"
        let node_name = if let Some(bracket_pos) = line.find('[') {
            line[..bracket_pos].trim().trim_matches('"').to_string()
        } else {
            continue; // not a node definition
        };

        if !node_name.is_empty() && node_name != "graph" && node_name != "node" && node_name != "edge" {
            v.nodes.push(node_name);
        }
    }

    // Validation checks
    if v.nodes.is_empty() {
        v.errors.push("no nodes defined in digraph".into());
    }

    // Check all edge endpoints reference defined nodes
    let node_set: HashSet<&str> = v.nodes.iter().map(|s| s.as_str()).collect();
    for (src, dst) in &v.edges {
        if !node_set.contains(src.as_str()) {
            v.errors.push(format!("edge source '{}' is not a defined node", src));
        }
        if !node_set.contains(dst.as_str()) {
            v.errors.push(format!("edge target '{}' is not a defined node", dst));
        }
    }

    // Check for cycles (simple DFS)
    if v.errors.is_empty() && !v.edges.is_empty() {
        let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
        for (src, dst) in &v.edges {
            adj.entry(src.as_str()).or_default().push(dst.as_str());
        }
        let mut visited = HashSet::new();
        let mut in_stack = HashSet::new();
        for node in &v.nodes {
            if has_cycle(node, &adj, &mut visited, &mut in_stack) {
                v.errors.push(format!("cycle detected involving node '{}'", node));
                break;
            }
        }
    }

    // Check connectivity: nodes without edges (except single-node graphs)
    if v.nodes.len() > 1 && v.edges.is_empty() {
        v.errors.push("multiple nodes but no edges — graph is disconnected".into());
    }

    v
}

fn has_cycle<'a>(
    node: &'a str,
    adj: &HashMap<&'a str, Vec<&'a str>>,
    visited: &mut HashSet<&'a str>,
    in_stack: &mut HashSet<&'a str>,
) -> bool {
    if in_stack.contains(node) {
        return true;
    }
    if visited.contains(node) {
        return false;
    }
    visited.insert(node);
    in_stack.insert(node);
    if let Some(neighbors) = adj.get(node) {
        for &next in neighbors {
            if has_cycle(next, adj, visited, in_stack) {
                return true;
            }
        }
    }
    in_stack.remove(node);
    false
}

// ── DOT attribute manipulation ──────────────────────────────────

/// Find the byte position of a node definition in the DOT string.
fn find_node(dot: &str, node_name: &str) -> Option<usize> {
    for pattern in &[
        format!("{node_name} ["),
        format!("{node_name}["),
        format!("\"{node_name}\" ["),
    ] {
        if let Some(pos) = dot.find(pattern.as_str()) {
            return Some(pos);
        }
    }
    None
}

/// Find the byte range of an attribute's quoted value within a node definition.
fn find_attr_value(dot: &str, node_start: usize, attr: &str) -> Option<std::ops::Range<usize>> {
    let node_def = &dot[node_start..];
    let end = node_def.find(']')?;
    let node_def = &node_def[..end];

    let pattern = format!("{attr}=");
    let attr_pos = node_def.find(&pattern)?;
    let value_start = node_start + attr_pos + pattern.len();

    let rest = &dot[value_start..];
    if rest.starts_with('"') {
        let mut i = 1;
        while i < rest.len() {
            if rest.as_bytes()[i] == b'"' && (i == 0 || rest.as_bytes()[i - 1] != b'\\') {
                return Some(value_start..value_start + i + 1);
            }
            i += 1;
        }
    }
    None
}

/// Get the value of a node attribute (without quotes).
fn get_attr(dot: &str, node_name: &str, attr: &str) -> Option<String> {
    let pos = find_node(dot, node_name)?;
    let range = find_attr_value(dot, pos, attr)?;
    Some(dot[range].trim_matches('"').to_string())
}

// ── Main ────────────────────────────────────────────────────────

fn main() {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap_or_default();

    let payload: HookPayload = match serde_json::from_str(&input) {
        Ok(p) => p,
        Err(_) => std::process::exit(0),
    };

    if payload.tool_name.as_deref() != Some("run_pipeline") {
        std::process::exit(0);
    }

    let tool_input = match payload.tool_input {
        Some(v) => v,
        None => std::process::exit(0),
    };

    let pipeline_str = tool_input.get("pipeline").and_then(|v| v.as_str()).unwrap_or("");
    if !pipeline_str.contains("digraph") {
        std::process::exit(0);
    }

    // ── Step 1: Validate DOT structure ──────────────────────────
    let validation = validate_dot(pipeline_str);
    if !validation.errors.is_empty() {
        let reasons = validation.errors.join("; ");
        println!("Invalid DOT graph: {reasons}");
        std::process::exit(1); // deny — LLM should regenerate
    }

    // ── Step 2: Load model metadata ─────────────────────────────
    let models: Vec<ModelMeta> = std::env::var("PIPELINE_GUARD_MODELS")
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    if models.is_empty() {
        std::process::exit(0); // no metadata, can't fix models
    }

    // Build model lookup
    let model_map: HashMap<&str, &ModelMeta> = models.iter().map(|m| (m.key.as_str(), m)).collect();
    let healthy_models: Vec<&ModelMeta> = models.iter().filter(|m| m.qos > 0.3).collect();

    if healthy_models.is_empty() {
        std::process::exit(0); // all models degraded, let it through
    }

    // Pick optimal models by role
    let best_search = healthy_models.iter().min_by_key(|m| m.max_output).unwrap();
    let best_synth = healthy_models.iter().max_by_key(|m| m.max_output).unwrap();
    let best_strong = healthy_models.iter().max_by_key(|m| m.context).unwrap();

    // ── Step 3: Validate referenced models exist ────────────────
    let mut dot = pipeline_str.to_string();
    let mut modified = false;

    for node_name in &validation.nodes {
        if let Some(model) = get_attr(&dot, node_name, "model") {
            if !model_map.contains_key(model.as_str()) {
                // Model doesn't exist — replace with best available
                eprintln!("[pipeline-guard] unknown model '{}' on node '{}', replacing", model, node_name);
                if let Some(pos) = find_node(&dot, node_name) {
                    if let Some(range) = find_attr_value(&dot, pos, "model") {
                        dot.replace_range(range, &format!("\"{}\"", best_strong.key));
                        modified = true;
                    }
                }
            } else {
                // Model exists — check QoS
                let meta = model_map[model.as_str()];
                if meta.qos < 0.3 {
                    eprintln!("[pipeline-guard] degraded model '{}' (QoS={:.2}) on node '{}', replacing", model, meta.qos, node_name);
                    if let Some(pos) = find_node(&dot, node_name) {
                        if let Some(range) = find_attr_value(&dot, pos, "model") {
                            dot.replace_range(range, &format!("\"{}\"", best_strong.key));
                            modified = true;
                        }
                    }
                }
            }
        }
    }

    // ── Step 4: Fix tools, models, and limits on nodes ────────────
    // Each fix re-finds the node to handle range invalidation from prior edits.

    // Helper: apply a single attribute fix on a node, re-finding position each time.
    fn fix_attr(dot: &mut String, node: &str, attr: &str, new_val: &str) -> bool {
        if let Some(pos) = find_node(dot, node) {
            if let Some(range) = find_attr_value(dot, pos, attr) {
                let current = dot[range.clone()].to_string();
                let quoted_new = format!("\"{}\"", new_val);
                if current != quoted_new {
                    dot.replace_range(range, &quoted_new);
                    return true;
                }
            }
        }
        false
    }

    fn get_current_attr(dot: &str, node: &str, attr: &str) -> Option<String> {
        let pos = find_node(dot, node)?;
        let range = find_attr_value(dot, pos, attr)?;
        Some(dot[range].trim_matches('"').to_string())
    }

    // Fix analyze: tools
    if let Some(tools) = get_current_attr(&dot, "analyze", "tools") {
        if tools.is_empty() || !tools.contains("read_file") {
            modified |= fix_attr(&mut dot, "analyze", "tools", "read_file,list_dir,glob");
        }
    }

    // Fix analyze: model (use strong reasoner)
    if let Some(model) = get_current_attr(&dot, "analyze", "model") {
        let meta = model_map.get(model.as_str());
        if meta.map(|m| m.qos < 0.5).unwrap_or(true) {
            modified |= fix_attr(&mut dot, "analyze", "model", &best_strong.key);
        }
    }

    // Fix synthesize: tools
    if let Some(tools) = get_current_attr(&dot, "synthesize", "tools") {
        if !tools.contains("read_file") {
            modified |= fix_attr(&mut dot, "synthesize", "tools", "read_file,write_file");
        }
    }

    // Fix synthesize: model (use highest max_output)
    if let Some(model) = get_current_attr(&dot, "synthesize", "model") {
        let meta = model_map.get(model.as_str());
        let should_replace = meta
            .map(|m| m.max_output < best_synth.max_output / 2 || m.qos < 0.5)
            .unwrap_or(true);
        if should_replace {
            modified |= fix_attr(&mut dot, "synthesize", "model", &best_synth.key);
        }
    }

    // Fix synthesize: max_output_tokens
    if let Some(mot) = get_current_attr(&dot, "synthesize", "max_output_tokens") {
        let current_val: u32 = mot.parse().unwrap_or(0);
        if current_val < best_synth.max_output {
            modified |= fix_attr(&mut dot, "synthesize", "max_output_tokens", &best_synth.max_output.to_string());
        }
    }

    if !modified {
        std::process::exit(0);
    }

    // ── Step 5: Re-validate after modifications ─────────────────
    let recheck = validate_dot(&dot);
    if !recheck.errors.is_empty() {
        eprintln!("[pipeline-guard] DOT became invalid after fixes: {:?}", recheck.errors);
        std::process::exit(0); // don't make things worse, let original through
    }

    // ── Output modified args ────────────────────────────────────
    let mut new_input = tool_input.clone();
    if let Some(obj) = new_input.as_object_mut() {
        obj.insert("pipeline".to_string(), serde_json::Value::String(dot));
    }

    println!("{}", serde_json::to_string(&new_input).unwrap());
    std::process::exit(2);
}
