//! Pipeline DOT graph guard — a before_tool_call hook for `run_pipeline`.
//!
//! Protocol:
//!   - Reads hook payload JSON from stdin (contains tool_name + arguments)
//!   - Validates the DOT graph structure
//!   - Resolves STRONG/FAST placeholders to actual models using model_catalog.json
//!   - Exit 0 = allow (DOT is fine, no changes needed)
//!   - Exit 2 = modified (stdout = corrected tool args JSON)
//!   - Exit 1 = deny (stdout = reason)
//!
//! Model resolution from model_catalog.json:
//!   - STRONG → best "strong" model (lowest score from AdaptiveRouter, prefer cheaper)
//!   - FAST → best "fast" model (lowest score, prefer cheaper)

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Read;

#[derive(Deserialize)]
struct HookPayload {
    tool_name: Option<String>,
    arguments: Option<serde_json::Value>,
}

// ── Model catalog ─────────────────────────────────────────────

#[derive(Deserialize, Serialize)]
struct ModelCatalog {
    models: Vec<CatalogEntry>,
}

#[derive(Deserialize, Serialize, Clone)]
struct CatalogEntry {
    /// Format: "provider/model" e.g. "minimax/MiniMax-M2.7"
    provider: String,
    /// "strong" or "fast"
    #[serde(rename = "type")]
    model_type: String,
    #[serde(default)]
    stability: f64,
    #[serde(default)]
    tool_avg_ms: u64,
    #[serde(default)]
    p95_ms: u64,
    #[serde(default)]
    score: f64,
    #[serde(default)]
    cost_in: f64,
    #[serde(default)]
    cost_out: f64,
    #[serde(default)]
    ds_output: u64,
    #[serde(default)]
    context_window: u64,
    #[serde(default)]
    max_output: u64,
}

impl CatalogEntry {
    /// Extract the model key (everything after the first slash).
    /// "nvidia/minimaxai/minimax-m2.5" → "minimaxai/minimax-m2.5"
    /// "minimax/MiniMax-M2.7" → "MiniMax-M2.7"
    fn model_key(&self) -> &str {
        match self.provider.split_once('/') {
            Some((_, rest)) => rest,
            None => &self.provider,
        }
    }
}

struct ModelPicks {
    /// STRONG models sorted by QoS (best first), for round-robin.
    strong_pool: Vec<String>,
    /// FAST models sorted by QoS (best first), for round-robin.
    fast_pool: Vec<String>,
    strong_idx: std::cell::Cell<usize>,
    fast_idx: std::cell::Cell<usize>,
}

impl ModelPicks {
    fn next_strong(&self) -> &str {
        let i = self.strong_idx.get();
        self.strong_idx.set(i + 1);
        &self.strong_pool[i % self.strong_pool.len()]
    }

    fn next_fast(&self) -> &str {
        let i = self.fast_idx.get();
        self.fast_idx.set(i + 1);
        &self.fast_pool[i % self.fast_pool.len()]
    }
}

// ── Profile config (to know which models are configured) ─────
//
// Schema mirrors the live `~/.octos/profiles/<id>.json` files:
//
//   {
//     "config": {
//       "llm": {
//         "primary":   { "family_id": "...", "model_id": "...", "route": {...} },
//         "fallbacks": [
//           { "family_id": "...", "model_id": "...", "route": {...} },
//           ...
//         ]
//       },
//       ...
//     }
//   }
//
// All fields outside `family_id`/`model_id` are tolerated and ignored.

#[derive(Deserialize)]
struct ProfileConfig {
    config: ProfileInner,
}

#[derive(Deserialize)]
struct ProfileInner {
    #[serde(default)]
    llm: Option<LlmConfig>,
}

#[derive(Deserialize)]
struct LlmConfig {
    #[serde(default)]
    primary: Option<ModelEntry>,
    #[serde(default)]
    fallbacks: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    // Both fields are Option<> to mirror the canonical
    // `LlmModelSelectionConfig` in `octos-cli/src/profiles.rs` — a primary or
    // fallback entry may legitimately omit family_id while keeping model_id
    // (or vice versa) without rejecting the whole profile.
    #[serde(default)]
    family_id: Option<String>,
    #[serde(default)]
    model_id: Option<String>,
    // Other fields like `route`, `strong`, `label`, `cost_per_m`, `model_hints`
    // are tolerated by serde unless `#[serde(deny_unknown_fields)]` is added —
    // keep it permissive so future schema additions don't break the guard.
}

/// Discover candidate profile JSON paths.
///
/// Order:
///   1. `$OCTOS_PROFILE` (full path to a profile JSON, set by the managed gateway)
///   2. `*.json` under `~/.octos/profiles/`
///   3. `*.json` under `~/.crew/profiles/`
fn discover_profile_paths() -> Vec<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut paths = Vec::new();

    if let Ok(profile_path) = std::env::var("OCTOS_PROFILE") {
        paths.push(profile_path);
    }

    for base in &[
        format!("{home}/.octos/profiles"),
        format!("{home}/.crew/profiles"),
    ] {
        if let Ok(entries) = std::fs::read_dir(base) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "json").unwrap_or(false) {
                    paths.push(path.to_string_lossy().to_string());
                }
            }
        }
    }

    paths
}

/// Resolve `family_id`/`model_id` pairs from a parsed profile into a flat
/// HashSet that includes BOTH:
///   - `format!("{family_id}/{model_id}")` — fully-qualified key
///   - `model_id`                          — bare model name
///
/// Both forms are inserted because pipeline-guard's STRONG/FAST advertisement
/// uses bare ids while the ProviderRouter sometimes registers under the
/// qualified key. Inserting both ensures whatever pipeline-guard injects
/// matches what the ProviderRouter has registered.
fn collect_models_from_profile(profile: &ProfileConfig, out: &mut HashSet<String>) {
    let Some(llm) = profile.config.llm.as_ref() else {
        return;
    };
    let mut insert_entry = |entry: &ModelEntry| {
        // Only meaningful when there's a model_id — bare family_id is not a
        // routing key the ProviderRouter understands.
        if let Some(model_id) = entry.model_id.as_deref() {
            if let Some(family_id) = entry.family_id.as_deref() {
                out.insert(format!("{family_id}/{model_id}"));
            }
            out.insert(model_id.to_string());
        }
    };
    if let Some(primary) = llm.primary.as_ref() {
        insert_entry(primary);
    }
    for fb in &llm.fallbacks {
        insert_entry(fb);
    }
}

/// Read the given profile JSON paths, parse, and collect every configured
/// model into a flat HashSet. Returns the first non-empty set found.
fn load_profile_models_from_paths(paths: &[String]) -> HashSet<String> {
    let mut models = HashSet::new();

    for path in paths {
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(profile) = serde_json::from_str::<ProfileConfig>(&content) else {
            continue;
        };
        collect_models_from_profile(&profile, &mut models);
        if !models.is_empty() {
            eprintln!(
                "[pipeline-guard] loaded {} models from profile {path}",
                models.len()
            );
            return models;
        }
    }
    models
}

/// Load the profile's configured models from disk.
/// Returns family/model keys like "moonshot/kimi-k2.5", plus bare ids
/// like "kimi-k2.5".
fn load_profile_models() -> HashSet<String> {
    load_profile_models_from_paths(&discover_profile_paths())
}

/// Load the system-wide model_catalog.json, filter to profile's available models,
/// and save the filtered copy to the profile's data dir.
fn load_catalog() -> Option<ModelCatalog> {
    let home = std::env::var("HOME").unwrap_or_default();
    let profile_data_dir = std::env::var("OCTOS_DATA_DIR").ok();

    // 1. Try guard's own filtered catalog (not touched by gateway's export)
    if let Some(ref data_dir) = profile_data_dir {
        let path = format!("{data_dir}/pipeline_models.json");
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(catalog) = serde_json::from_str::<ModelCatalog>(&content) {
                let has_qos = catalog.models.iter().any(|m| m.ds_output > 0);
                if !catalog.models.is_empty() && has_qos {
                    eprintln!("[pipeline-guard] loaded pipeline models from {path}");
                    return Some(catalog);
                }
            }
        }
    }

    // 2. Load system-wide catalog
    let system_catalog = load_system_catalog(&home)?;

    // 3. Filter by profile's available models.
    //
    // FAIL CLOSED: when no profile models are discoverable, or when no
    // catalog entries match the profile, we must NOT fall back to the
    // full catalog. The full-catalog fallback was the root cause of the
    // pipeline-guard injection bug (live-thread-interleave:207): it
    // advertised models like `qwen3.5-plus` that the profile-scoped
    // ProviderRouter never registered, causing pipelines to die in 0 ms
    // with `no provider registered for key '…'`. Returning None here
    // makes main() pass through (exit 0) — the LLM's original DOT runs
    // unmodified, which is the safest behavior when guard cannot reason
    // about the profile.
    let profile_models = load_profile_models();
    if profile_models.is_empty() {
        eprintln!(
            "[pipeline-guard] no profile models found — passing through \
             (full-catalog fallback removed; see fix/pipeline-guard-profile-schema)"
        );
        return None;
    }

    let filtered_models: Vec<CatalogEntry> = system_catalog
        .models
        .into_iter()
        .filter(|entry| {
            let model_key = entry.model_key();
            // Match by model key (part after slash) against profile's model names
            // e.g. catalog "nvidia/minimax-m2.5" matches metrics "minimaxai/minimax-m2.5"
            // by checking if any profile model contains the catalog model key
            profile_models.iter().any(|pm| {
                pm == model_key
                    || pm.ends_with(model_key)
                    || model_key.ends_with(pm.split('/').next_back().unwrap_or(pm))
            })
        })
        .collect();

    eprintln!(
        "[pipeline-guard] filtered catalog: {}/{} models match profile",
        filtered_models.len(),
        profile_models.len()
    );

    if filtered_models.is_empty() {
        // Same fail-closed reasoning as above: profile is known but no
        // catalog entry matches → don't risk advertising unregistered
        // models. Pass through.
        eprintln!(
            "[pipeline-guard] no catalog models match profile — passing through \
             (full-catalog fallback removed)"
        );
        return None;
    }

    let filtered_catalog = ModelCatalog {
        models: filtered_models,
    };

    // 4. Save filtered copy as pipeline_models.json (separate from gateway's model_catalog.json)
    if let Some(ref data_dir) = profile_data_dir {
        let path = format!("{data_dir}/pipeline_models.json");
        if let Ok(json) = serde_json::to_string_pretty(&filtered_catalog) {
            let _ = std::fs::write(&path, &json);
            eprintln!("[pipeline-guard] saved pipeline models to {path}");
        }
    }

    Some(filtered_catalog)
}

/// Load the system-wide model_catalog.json from standard locations.
fn load_system_catalog(home: &str) -> Option<ModelCatalog> {
    for path in &[
        format!("{home}/.octos/model_catalog.json"),
        format!("{home}/.crew/model_catalog.json"),
        "model_catalog.json".to_string(),
    ] {
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Ok(catalog) = serde_json::from_str::<ModelCatalog>(&content) {
                eprintln!("[pipeline-guard] loaded system catalog from {path}");
                return Some(catalog);
            }
        }
    }
    None
}

/// Pick STRONG and FAST models from the catalog.
///
/// Same logic for both: best QoS (score) first, cheaper as fallback.
/// - Primary: highest score among healthy models of the matching type
/// - If scores are equal (or zero), prefer cheaper (lower cost_out)
/// - If no models of the requested type, fall back to the other type
fn pick_models(catalog: &ModelCatalog) -> Option<ModelPicks> {
    let strong_models: Vec<&CatalogEntry> = catalog
        .models
        .iter()
        .filter(|m| m.model_type == "strong" && m.stability > 0.5)
        .collect();

    let fast_models: Vec<&CatalogEntry> = catalog
        .models
        .iter()
        .filter(|m| m.model_type == "fast" && m.stability > 0.5)
        .collect();

    // Best model = lowest score (score is lower-is-better from AdaptiveRouter).
    // Tiebreak: cheaper wins.
    #[allow(dead_code)]
    fn best_of<'a>(models: &[&'a CatalogEntry]) -> Option<&'a CatalogEntry> {
        models.iter().copied().min_by(|a, b| {
            a.score
                .partial_cmp(&b.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(
                    a.cost_out
                        .partial_cmp(&b.cost_out)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
        })
    }

    // Sort both pools by score ascending (lower = better), cheaper tiebreak
    let mut strong_sorted = strong_models.clone();
    strong_sorted.sort_by(|a, b| {
        a.score
            .partial_cmp(&b.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                a.cost_out
                    .partial_cmp(&b.cost_out)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });

    let mut fast_sorted = fast_models.clone();
    fast_sorted.sort_by(|a, b| {
        a.score
            .partial_cmp(&b.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                a.cost_out
                    .partial_cmp(&b.cost_out)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });

    // Use model key for DOT injection (the ProviderRouter resolves via
    // last-resort suffix matching). Full provider/model keys with '/' can
    // break the DOT parser's attribute handling.
    let strong_pool: Vec<String> = strong_sorted
        .iter()
        .map(|e| e.model_key().to_string())
        .collect();
    let fast_pool: Vec<String> = fast_sorted
        .iter()
        .map(|e| e.model_key().to_string())
        .collect();

    // Fall back to each other if one pool is empty
    if strong_pool.is_empty() && fast_pool.is_empty() {
        return None;
    }
    let strong_pool = if strong_pool.is_empty() {
        fast_pool.clone()
    } else {
        strong_pool
    };
    let fast_pool = if fast_pool.is_empty() {
        strong_pool.clone()
    } else {
        fast_pool
    };

    // Random start so concurrent pipelines get different models.
    // Use PID to differentiate — each pipeline hook runs as a separate process.
    let pid = std::process::id() as usize;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as usize;
    let seed = pid.wrapping_mul(7) ^ nanos;

    Some(ModelPicks {
        strong_pool,
        fast_pool,
        strong_idx: std::cell::Cell::new(seed),
        fast_idx: std::cell::Cell::new(seed.wrapping_mul(13)),
    })
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

    let trimmed = dot.trim();
    if !trimmed.starts_with("digraph") {
        v.errors.push("DOT must start with 'digraph'".into());
        return v;
    }

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

    for raw_line in body.split('\n') {
        let line = raw_line.trim().trim_end_matches(';').trim();
        if line.is_empty() || line.starts_with("//") || line.starts_with('#') {
            continue;
        }

        if line.contains("->") {
            let parts: Vec<&str> = line.split("->").collect();
            if parts.len() == 2 {
                let src = parts[0].trim().trim_matches('"').to_string();
                let dst = parts[1]
                    .trim()
                    .split('[')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_matches('"')
                    .to_string();
                if !src.is_empty() && !dst.is_empty() {
                    v.edges.push((src, dst));
                }
            }
            continue;
        }

        let node_name = if let Some(bracket_pos) = line.find('[') {
            line[..bracket_pos].trim().trim_matches('"').to_string()
        } else {
            continue;
        };

        if !node_name.is_empty()
            && node_name != "graph"
            && node_name != "node"
            && node_name != "edge"
        {
            v.nodes.push(node_name);
        }
    }

    if v.nodes.is_empty() {
        v.errors.push("no nodes defined in digraph".into());
    }

    let node_set: HashSet<&str> = v.nodes.iter().map(|s| s.as_str()).collect();
    for (src, dst) in &v.edges {
        if !node_set.contains(src.as_str()) {
            v.errors
                .push(format!("edge source '{src}' is not a defined node"));
        }
        if !node_set.contains(dst.as_str()) {
            v.errors
                .push(format!("edge target '{dst}' is not a defined node"));
        }
    }

    if v.errors.is_empty() && !v.edges.is_empty() {
        let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
        for (src, dst) in &v.edges {
            adj.entry(src.as_str()).or_default().push(dst.as_str());
        }
        let mut visited = HashSet::new();
        let mut in_stack = HashSet::new();
        for node in &v.nodes {
            if has_cycle(node, &adj, &mut visited, &mut in_stack) {
                v.errors
                    .push(format!("cycle detected involving node '{node}'"));
                break;
            }
        }
    }

    if v.nodes.len() > 1 && v.edges.is_empty() {
        v.errors
            .push("multiple nodes but no edges — graph is disconnected".into());
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

fn fix_attr(dot: &mut String, node: &str, attr: &str, new_val: &str) -> bool {
    if let Some(pos) = find_node(dot, node) {
        if let Some(range) = find_attr_value(dot, pos, attr) {
            let current = dot[range.clone()].to_string();
            let quoted_new = format!("\"{new_val}\"");
            if current != quoted_new {
                dot.replace_range(range, &quoted_new);
                return true;
            }
        }
    }
    false
}

fn get_attr(dot: &str, node_name: &str, attr: &str) -> Option<String> {
    let pos = find_node(dot, node_name)?;
    let range = find_attr_value(dot, pos, attr)?;
    Some(dot[range].trim_matches('"').to_string())
}

// ── Main ────────────────────────────────────────────────────────

fn main() {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .unwrap_or_default();

    let payload: HookPayload = match serde_json::from_str(&input) {
        Ok(p) => p,
        Err(_) => std::process::exit(0),
    };

    if payload.tool_name.as_deref() != Some("run_pipeline") {
        std::process::exit(0);
    }

    let arguments = match payload.arguments {
        Some(v) => v,
        None => std::process::exit(0),
    };

    let pipeline_str = arguments
        .get("pipeline")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !pipeline_str.contains("digraph") {
        std::process::exit(0);
    }

    // ── Step 1: Validate DOT structure ──────────────────────────
    let validation = validate_dot(pipeline_str);
    if !validation.errors.is_empty() {
        let reasons = validation.errors.join("; ");
        println!("Invalid DOT graph: {reasons}");
        std::process::exit(1);
    }

    // ── Step 2: Load model catalog and pick models ──────────────
    // load_catalog() returns None for any of:
    //   - no system-wide model_catalog.json on disk
    //   - no profile models discoverable (would have caused the
    //     historical "no profile models found, using full catalog" bug)
    //   - profile models discoverable but no catalog entries match
    // In every "None" case we pass through unchanged — the LLM's DOT
    // runs as-written, which is safer than injecting models the
    // ProviderRouter cannot resolve.
    let catalog = match load_catalog() {
        Some(c) => c,
        None => {
            eprintln!(
                "[pipeline-guard] catalog/profile not resolvable — passing through unchanged"
            );
            std::process::exit(0);
        }
    };

    let picks = match pick_models(&catalog) {
        Some(p) => p,
        None => {
            eprintln!("[pipeline-guard] no healthy models in catalog, passing through");
            std::process::exit(0);
        }
    };

    eprintln!(
        "[pipeline-guard] resolved STRONG=[{}], FAST=[{}]",
        picks.strong_pool.join(", "),
        picks.fast_pool.join(", ")
    );

    // ── Step 3: Inject/replace model= on all nodes ────────────────
    // The hook owns model selection. LLM just writes prompts and structure.
    // - dynamic_parallel / *search* nodes → FAST
    // - everything else (analyze, synthesize, etc.) → STRONG
    // - planner_model on dynamic_parallel → STRONG
    let mut dot = pipeline_str.to_string();
    let mut modified = false;

    for node_name in &validation.nodes {
        let is_fast = get_attr(&dot, node_name, "handler")
            .map(|h| h == "dynamic_parallel")
            .unwrap_or(false)
            || node_name.contains("search");

        // For dynamic_parallel nodes (spawns multiple workers), inject the full
        // FAST pool as comma-separated so the executor can round-robin per worker.
        // For other nodes, inject a single model.
        let is_dynamic_parallel = get_attr(&dot, node_name, "handler")
            .map(|h| h == "dynamic_parallel")
            .unwrap_or(false);

        let target_owned;
        let target = if is_fast && is_dynamic_parallel {
            // Full pool for worker distribution
            target_owned = picks.fast_pool.join(",");
            target_owned.as_str()
        } else if is_fast {
            target_owned = picks.next_fast().to_string();
            target_owned.as_str()
        } else {
            target_owned = picks.next_strong().to_string();
            target_owned.as_str()
        };

        if let Some(model) = get_attr(&dot, node_name, "model") {
            // Replace existing model=
            if model != target {
                eprintln!(
                    "[pipeline-guard] node '{}': model '{}' -> '{}'",
                    node_name, model, target
                );
                modified |= fix_attr(&mut dot, node_name, "model", target);
            }
        } else {
            // No model= attribute — inject one
            if let Some(pos) = find_node(&dot, node_name) {
                // Find the opening '[' and inject model= right after it
                if let Some(bracket) = dot[pos..].find('[') {
                    let insert_pos = pos + bracket + 1;
                    let injection = format!("model=\"{target}\", ");
                    dot.insert_str(insert_pos, &injection);
                    eprintln!(
                        "[pipeline-guard] node '{}': injected model='{}'",
                        node_name, target
                    );
                    modified = true;
                }
            }
        }

        // Inject/replace planner_model on dynamic_parallel nodes
        if get_attr(&dot, node_name, "handler")
            .map(|h| h == "dynamic_parallel")
            .unwrap_or(false)
        {
            let planner = picks.next_strong().to_string();
            if let Some(pm) = get_attr(&dot, node_name, "planner_model") {
                if pm != planner {
                    eprintln!(
                        "[pipeline-guard] node '{}': planner_model '{}' -> '{}'",
                        node_name, pm, planner
                    );
                    modified |= fix_attr(&mut dot, node_name, "planner_model", &planner);
                }
            } else {
                // Inject planner_model
                if let Some(pos) = find_node(&dot, node_name) {
                    if let Some(bracket) = dot[pos..].find('[') {
                        let insert_pos = pos + bracket + 1;
                        let injection = format!("planner_model=\"{planner}\", ");
                        dot.insert_str(insert_pos, &injection);
                        eprintln!(
                            "[pipeline-guard] node '{}': injected planner_model='{}'",
                            node_name, planner
                        );
                        modified = true;
                    }
                }
            }
        }
    }

    if !modified {
        std::process::exit(0);
    }

    // ── Step 4: Re-validate after modifications ─────────────────
    let recheck = validate_dot(&dot);
    if !recheck.errors.is_empty() {
        eprintln!(
            "[pipeline-guard] DOT became invalid after fixes: {:?}",
            recheck.errors
        );
        std::process::exit(0);
    }

    // ── Output modified args ────────────────────────────────────
    let mut new_args = arguments.clone();
    if let Some(obj) = new_args.as_object_mut() {
        obj.insert("pipeline".to_string(), serde_json::Value::String(dot));
    }

    eprintln!("[pipeline-guard] DOT modified with resolved models");
    println!("{}", serde_json::to_string(&new_args).unwrap());
    std::process::exit(2);
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    fn load_fixture(name: &str) -> HashSet<String> {
        let path = format!("{FIXTURE_DIR}/{name}");
        load_profile_models_from_paths(&[path])
    }

    /// Real-shape `dspfac.json` profile (sanitized). Verifies pipeline-guard
    /// reads `config.llm.primary` + `config.llm.fallbacks` correctly and
    /// returns BOTH fully-qualified keys (family_id/model_id) AND bare
    /// model_ids — both forms are needed so whatever pipeline-guard injects
    /// matches whatever the ProviderRouter has registered.
    #[test]
    fn pipeline_guard_loads_models_from_real_profile_schema() {
        let models = load_fixture("dspfac.json");

        // Bare model_ids — what pipeline-guard's STRONG/FAST advertisement uses
        assert!(
            models.contains("kimi-k2.5"),
            "expected bare primary model_id in {models:?}"
        );
        assert!(
            models.contains("MiniMax-M2.5-highspeed"),
            "expected bare fallback model_id in {models:?}"
        );
        assert!(
            models.contains("DeepSeek-V3.2"),
            "expected bare fallback model_id in {models:?}"
        );

        // Fully-qualified family_id/model_id — what the ProviderRouter
        // registers in some configurations.
        assert!(
            models.contains("moonshot/kimi-k2.5"),
            "expected qualified primary key in {models:?}"
        );
        assert!(
            models.contains("minimax/MiniMax-M2.5-highspeed"),
            "expected qualified fallback key in {models:?}"
        );
        assert!(
            models.contains("deepseek/DeepSeek-V3.2"),
            "expected qualified fallback key in {models:?}"
        );

        // Exactly 6 entries: 3 bare + 3 qualified
        assert_eq!(models.len(), 6, "unexpected model set: {models:?}");
    }

    /// Old-style profile without an `llm` section returns empty set.
    /// Ensures pipeline-guard degrades to "no profile models found" rather
    /// than panicking on legacy/incomplete profile JSON.
    #[test]
    fn pipeline_guard_handles_missing_llm_section_gracefully() {
        let models = load_fixture("legacy_no_llm.json");
        assert!(
            models.is_empty(),
            "expected empty set for profile without llm section, got {models:?}"
        );
    }

    /// Profile with `llm.primary` but no `llm.fallbacks` returns just the
    /// primary's family/bare pair.
    #[test]
    fn pipeline_guard_handles_missing_fallbacks_array() {
        let models = load_fixture("primary_only.json");
        assert!(models.contains("kimi-k2.5"));
        assert!(models.contains("moonshot/kimi-k2.5"));
        assert_eq!(models.len(), 2, "expected only primary entries: {models:?}");
    }

    /// Sanity check that an unknown route field on a fallback (e.g. `strong: true`)
    /// does not break parsing — serde tolerance is intentional.
    #[test]
    fn pipeline_guard_tolerates_unknown_fields_on_model_entries() {
        let models = load_fixture("dspfac.json");
        // The DeepSeek fallback in the fixture carries a `strong: true` field;
        // if we accidentally added `deny_unknown_fields`, parsing would fail
        // and DeepSeek would be missing from the set.
        assert!(
            models.contains("DeepSeek-V3.2"),
            "deny_unknown_fields likely re-introduced — fixture has unknown 'strong' field"
        );
    }

    /// Mirrors the canonical `LlmModelSelectionConfig` schema where
    /// `family_id` and `model_id` are both `Option<String>`. A profile with
    /// only `model_id` should still yield a usable bare key — the qualified
    /// key is simply omitted.
    #[test]
    fn pipeline_guard_handles_optional_family_id() {
        let models = load_fixture("missing_family_id.json");
        assert!(
            models.contains("kimi-k2.5"),
            "expected bare model_id even without family_id in {models:?}"
        );
        // Without family_id, the qualified key should not appear.
        assert!(
            !models.iter().any(|m| m.ends_with("/kimi-k2.5")),
            "qualified key should be skipped when family_id absent: {models:?}"
        );
        assert_eq!(models.len(), 1);
    }

    /// A profile written in the OLD pre-fix struct shape (flat
    /// `config.provider`/`config.model`/`config.fallback_models`) must
    /// return EMPTY under the new schema-aware reader. Combined with the
    /// fail-closed `load_catalog()` change (which now returns None when
    /// `load_profile_models()` is empty), this guarantees pipeline-guard
    /// passes through unchanged for legacy profiles rather than silently
    /// advertising the full system catalog — the bug that broke
    /// live-thread-interleave:207.
    #[test]
    fn pipeline_guard_returns_empty_for_legacy_pre_fix_shape() {
        let models = load_fixture("legacy_pre_fix_shape.json");
        assert!(
            models.is_empty(),
            "legacy pre-fix shape must not be silently parsed; got {models:?}"
        );
    }

    /// Multiple paths: first non-empty wins. Confirms the iteration order
    /// in `load_profile_models_from_paths` keeps a parsed dspfac profile
    /// even if a legacy/empty profile precedes it.
    #[test]
    fn pipeline_guard_skips_empty_profile_and_keeps_searching() {
        let legacy = format!("{FIXTURE_DIR}/legacy_no_llm.json");
        let real = format!("{FIXTURE_DIR}/dspfac.json");
        let models = load_profile_models_from_paths(&[legacy, real]);
        assert!(
            models.contains("kimi-k2.5"),
            "expected to fall through legacy profile to dspfac fixture: {models:?}"
        );
    }
}
