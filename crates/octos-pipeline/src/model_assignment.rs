//! In-process model assignment for `run_pipeline` DOT graphs.
//!
//! Replaces the `pipeline-guard` plugin's `before_tool_call` hook with an
//! in-process Rust function that runs after `parse_dot` and assigns
//! `node.model` / `node.planner_model` for any node the LLM left unset,
//! using QoS scores from the profile's `model_catalog.json`.
//!
//! Why this lives here and not as a plugin:
//!
//! - **Correctness-critical** — without per-node model assignment, every
//!   pipeline node hits the same default model regardless of the strong/
//!   fast cost-quality trade-off. The historical plugin form ran via a
//!   manifest hook that has been observed to silently fail (manifest
//!   parse error → no model assignment → degraded pipeline). See
//!   `book/src/skill-development.md` "Before You Start: Skill vs.
//!   Workspace Contract" for the full decision rubric.
//! - **Pure Rust data ops on a parsed `PipelineGraph`** — no Python
//!   ecosystem, no native CLI, no per-tenant customization. The plugin's
//!   string-level DOT mutation is unnecessary once the graph is parsed.
//! - **Hot path** — runs on every `run_pipeline` invocation; an in-process
//!   function avoids the subprocess + JSON IPC overhead of the plugin
//!   form.
//!
//! Assignment rules (mirror of the historical plugin behavior so existing
//! profiles keep behaving the same):
//!
//! - `DynamicParallel` nodes (search planners) get the **full FAST pool**
//!   in `node.model` (comma-separated). The pipeline executor at
//!   `executor.rs::1800-1830` already round-robins workers across this
//!   pool. Their `planner_model` is set to a single STRONG model so the
//!   planning step itself is high-quality.
//! - Other LLM-handler nodes with `"search"` substring in their id get
//!   FAST (single).
//! - All other LLM-handler nodes (`analyze`, `synthesize`, etc.) get
//!   STRONG (single).
//! - Non-LLM nodes (`Shell`, `Gate`, `Noop`) are left untouched.
//! - Nodes the LLM ALREADY annotated with `model="..."` are NEVER
//!   overwritten — explicit operator/LLM choice wins.
//!
//! PID + nanos seeded round-robin start indices keep concurrent
//! pipelines from herd-routing to the same model on every call.

use std::collections::BTreeSet;
use std::path::Path;

use octos_llm::ProviderRouter;
use serde::Deserialize;

use crate::graph::{HandlerKind, PipelineGraph};

/// On-disk shape of `model_catalog.json` / `pipeline_models.json`.
///
/// Both files share the same wire shape — the difference is just which
/// one the operator updated last. `pipeline_models.json` is the
/// guard-specific filtered subset (profile-aware), while
/// `model_catalog.json` is the system-wide table. We prefer the filtered
/// copy when present.
#[derive(Debug, Deserialize)]
struct ModelCatalog {
    #[serde(default)]
    models: Vec<CatalogEntry>,
}

/// One row in the model catalog. Matches `octos_llm::QosCatalog` rows
/// modulo serialization-only fields (the LLM crate's authoritative type
/// is what gets serialized here; this is a permissive parse-side mirror).
#[derive(Debug, Deserialize, Clone)]
struct CatalogEntry {
    /// `"provider/model_id"` (the AdaptiveRouter's primary key) or
    /// occasionally a bare model id. We extract the model key from
    /// whichever shape lands on disk via [`Self::model_key`].
    provider: String,
    /// `"strong"` or `"fast"` — the cost/quality classification the
    /// AdaptiveRouter uses to bucket lanes.
    #[serde(rename = "type")]
    model_type: String,
    #[serde(default)]
    stability: f64,
    #[serde(default)]
    score: f64,
    #[serde(default)]
    cost_out: f64,
}

impl CatalogEntry {
    /// Extract the suffix after the first `'/'` as the routing key.
    /// `"moonshot/kimi-k2.5"` → `"kimi-k2.5"`. Bare entries pass through.
    fn model_key(&self) -> &str {
        match self.provider.split_once('/') {
            Some((_, rest)) => rest,
            None => &self.provider,
        }
    }
}

/// Resolved STRONG/FAST pools, with starting indices seeded from
/// PID + nanos so two concurrent pipelines don't both pick lane 0.
#[derive(Debug, Clone)]
struct ModelPools {
    strong: Vec<String>,
    fast: Vec<String>,
    strong_start: usize,
    fast_start: usize,
}

impl ModelPools {
    fn nth_strong(&self, offset: usize) -> Option<&str> {
        if self.strong.is_empty() {
            return None;
        }
        Some(&self.strong[(self.strong_start + offset) % self.strong.len()])
    }

    fn nth_fast(&self, offset: usize) -> Option<&str> {
        if self.fast.is_empty() {
            return None;
        }
        Some(&self.fast[(self.fast_start + offset) % self.fast.len()])
    }
}

/// Assign `model=` and `planner_model=` on every LLM-handler node that
/// the LLM left unset, using the catalog on disk at
/// `{data_dir}/pipeline_models.json` (preferred) or
/// `{data_dir}/model_catalog.json` (fallback).
///
/// **Never overwrites explicit `model=` attributes** — the LLM's or
/// operator's explicit choice takes precedence so this remains a default
/// rather than a policy.
///
/// When `router` is present, candidates are filtered to models the
/// `ProviderRouter` can actually `resolve` (#1901). The persisted
/// `model_catalog.json` is a deliberate superset (researched entries +
/// live scores), so without this filter a profile whose router registers
/// only lane keys (`cheap`/`strong`) gets nodes assigned catalog models
/// it can never resolve — the executor then silently degrades every such
/// node to the default provider. Filtering uses the router's own
/// resolution semantics (exact → case-insensitive → prefix split →
/// suffix alias), NOT `keys()` membership, so lane keys and aliases keep
/// passing; the pre-execution validator is intentionally not narrowed.
/// `None` (no router) fails open: assignment behaves as before.
///
/// Never blocks a pipeline: a missing catalog, no healthy models, or a
/// fully-unresolvable pool each leave the graph untouched (with a
/// `debug!` line). Tracing emits a single `info!` line indicating how
/// many nodes were touched so operators can confirm the assignment
/// fired without scraping per-node logs.
pub fn assign_from_catalog_dir(
    graph: &mut PipelineGraph,
    data_dir: &Path,
    router: Option<&ProviderRouter>,
) {
    let Some(catalog) = load_catalog(data_dir) else {
        tracing::debug!(
            data_dir = %data_dir.display(),
            "model_assignment: no catalog found, leaving DOT unchanged"
        );
        return;
    };

    let Some(pools) = build_pools(&catalog, router) else {
        tracing::debug!(
            "model_assignment: no usable strong/fast pool \
             (unhealthy or unresolvable by the provider router)"
        );
        return;
    };

    assign_to_graph(graph, &pools);
}

/// Return every model routing key present in the profile/system pipeline
/// catalog. This lets the pre-execution validator enforce explicit `model=`
/// attributes without doing I/O inside the pure validation pass.
pub fn known_model_keys_from_catalog_dir(data_dir: &Path) -> BTreeSet<String> {
    let Some(catalog) = load_catalog(data_dir) else {
        return BTreeSet::new();
    };
    catalog
        .models
        .iter()
        .flat_map(|entry| {
            let mut keys = vec![entry.provider.clone(), entry.model_key().to_string()];
            if let Some((provider, _)) = entry.provider.split_once('/') {
                keys.push(provider.to_string());
            }
            keys
        })
        .collect()
}

/// Test-friendly entry point that takes pre-built pools directly so we
/// can exercise the assignment logic without touching the filesystem.
///
/// `#[cfg(test)]` so `cargo clippy --workspace --all-targets -- -D warnings`
/// doesn't flag the lib-target as unused (the function is referenced only
/// from the inline `#[cfg(test)] mod tests` block below).
#[cfg(test)]
pub(crate) fn assign_with_pools_for_test(graph: &mut PipelineGraph, pools: ModelPoolsArg) {
    let pools = ModelPools {
        strong: pools.strong,
        fast: pools.fast,
        strong_start: 0,
        fast_start: 0,
    };
    assign_to_graph(graph, &pools);
}

/// Pre-built pool argument exposed to tests via `assign_with_pools_for_test`.
#[cfg(test)]
pub struct ModelPoolsArg {
    pub strong: Vec<String>,
    pub fast: Vec<String>,
}

fn load_catalog(data_dir: &Path) -> Option<ModelCatalog> {
    // Prefer the profile-filtered copy when present — it was filtered
    // down to models the ProviderRouter actually registered.
    let filtered = data_dir.join("pipeline_models.json");
    if let Ok(content) = std::fs::read_to_string(&filtered) {
        if let Ok(catalog) = serde_json::from_str::<ModelCatalog>(&content) {
            if !catalog.models.is_empty() {
                return Some(catalog);
            }
        }
    }
    // Fall back to the system-wide catalog.
    let system = data_dir.join("model_catalog.json");
    if let Ok(content) = std::fs::read_to_string(&system) {
        if let Ok(catalog) = serde_json::from_str::<ModelCatalog>(&content) {
            if !catalog.models.is_empty() {
                return Some(catalog);
            }
        }
    }
    None
}

fn build_pools(catalog: &ModelCatalog, router: Option<&ProviderRouter>) -> Option<ModelPools> {
    // Keep only entries with passable stability so we don't herd-route
    // to a broken provider. The 0.5 threshold matches the historical
    // plugin so behavior is preserved.
    let mut healthy: Vec<&CatalogEntry> = catalog
        .models
        .iter()
        .filter(|m| m.stability > 0.5)
        .collect();

    // #1901 Layer 1: an assignment is only useful if the executor can later
    // resolve it, so keep candidates to what THIS profile's ProviderRouter
    // resolves. The persisted `model_catalog.json` is a deliberate superset
    // (all researched entries + live scores for configured lanes), so an
    // unfiltered pool hands nodes models the router never registered — every
    // such node then silently degrades to the default provider at execution
    // time. The predicate is the router's own `resolve` (exact →
    // case-insensitive → prefix split → suffix alias), NOT `keys()`
    // membership: the router resolves lane keys and aliases that a literal
    // membership check rejects (#1904's defect). With no router, fail open —
    // assignment keeps using the catalog as-is.
    if let Some(router) = router {
        let before = healthy.len();
        healthy.retain(|m| router.resolve(m.model_key()).is_ok());
        let dropped = before - healthy.len();
        if dropped > 0 {
            // Sorted so repeated runs diff cleanly (`keys()` iterates a HashMap).
            let mut router_keys = router.keys();
            router_keys.sort();
            tracing::warn!(
                dropped,
                ?router_keys,
                "model_assignment: skipped healthy catalog models the provider router cannot resolve"
            );
        }
    }

    let mut strong: Vec<&CatalogEntry> = healthy
        .iter()
        .copied()
        .filter(|m| m.model_type == "strong")
        .collect();
    let mut fast: Vec<&CatalogEntry> = healthy
        .iter()
        .copied()
        .filter(|m| m.model_type == "fast")
        .collect();

    // Lower score = better lane per AdaptiveRouter; cheaper out price
    // breaks ties so we don't burn budget on equal-quality lanes.
    let order = |a: &&CatalogEntry, b: &&CatalogEntry| {
        a.score
            .partial_cmp(&b.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                a.cost_out
                    .partial_cmp(&b.cost_out)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    };
    strong.sort_by(order);
    fast.sort_by(order);

    let mut strong_keys: Vec<String> = strong.iter().map(|e| e.model_key().to_string()).collect();
    let mut fast_keys: Vec<String> = fast.iter().map(|e| e.model_key().to_string()).collect();

    if strong_keys.is_empty() && fast_keys.is_empty() {
        return None;
    }
    // Mirror plugin behavior: if one pool is empty, alias it to the
    // other so the assignment never hands a node a non-existent pool.
    if strong_keys.is_empty() {
        strong_keys = fast_keys.clone();
    }
    if fast_keys.is_empty() {
        fast_keys = strong_keys.clone();
    }

    // Seed round-robin starting positions so concurrent pipelines don't
    // all pick lane 0. Matches the plugin's PID-based seed (the plugin
    // ran as a separate process; here we still differentiate by nanos
    // since multiple in-process invocations would otherwise herd).
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as usize;
    let pid = std::process::id() as usize;
    let seed = pid.wrapping_mul(7) ^ nanos;
    let strong_start = seed % strong_keys.len();
    let fast_start = seed.wrapping_mul(13) % fast_keys.len();

    Some(ModelPools {
        strong: strong_keys,
        fast: fast_keys,
        strong_start,
        fast_start,
    })
}

fn assign_to_graph(graph: &mut PipelineGraph, pools: &ModelPools) {
    let mut strong_offset: usize = 0;
    let mut fast_offset: usize = 0;
    let mut assigned = 0usize;
    let mut skipped_explicit = 0usize;

    // Iterate by node id (sorted for deterministic offset assignment in tests).
    let mut ids: Vec<String> = graph.nodes.keys().cloned().collect();
    ids.sort();

    for id in ids {
        let Some(node) = graph.nodes.get_mut(&id) else {
            continue;
        };

        // Non-LLM handlers don't take a model attribute.
        if matches!(
            node.handler,
            HandlerKind::Shell | HandlerKind::Gate | HandlerKind::Noop
        ) {
            continue;
        }

        // Operator/LLM-provided model wins.
        if node.model.is_some() {
            skipped_explicit += 1;
            // Still consider injecting planner_model on DynamicParallel
            // when only `model=` was set (a rare authoring shape).
            if matches!(node.handler, HandlerKind::DynamicParallel) && node.planner_model.is_none()
            {
                if let Some(m) = pools.nth_strong(strong_offset) {
                    node.planner_model = Some(m.to_string());
                    strong_offset += 1;
                    assigned += 1;
                }
            }
            continue;
        }

        // DynamicParallel: comma-joined FAST pool so executor.rs's
        // round-robin (`model_pool[i % pool.len()]` at executor.rs:1830)
        // actually has more than one entry to choose from. The planner
        // step itself uses STRONG.
        if matches!(node.handler, HandlerKind::DynamicParallel) {
            if !pools.fast.is_empty() {
                node.model = Some(pools.fast.join(","));
                assigned += 1;
            }
            if node.planner_model.is_none() {
                if let Some(m) = pools.nth_strong(strong_offset) {
                    node.planner_model = Some(m.to_string());
                    strong_offset += 1;
                    assigned += 1;
                }
            }
            continue;
        }

        // Heuristic: id contains a retrieval-shaped verb → FAST pool.
        // Else: synthesis / analysis / generation → STRONG pool.
        //
        // The substring list is intentionally broad so operator-authored
        // pipelines that use natural verbs (e.g. `gather_news`,
        // `crawl_pages`, `investigate_topic`) still land on the FAST pool
        // without needing an explicit `model:` override.
        const FAST_HEURISTIC_SUBSTRINGS: &[&str] = &[
            "search",
            "retrieve",
            "gather",
            "collect",
            "crawl",
            "fetch",
            "survey",
            "investigate",
            "scan",
            "lookup",
            "browse",
        ];
        let id_lower = id.to_lowercase();
        let is_retrieval = FAST_HEURISTIC_SUBSTRINGS
            .iter()
            .any(|needle| id_lower.contains(needle));
        if is_retrieval {
            if let Some(m) = pools.nth_fast(fast_offset) {
                node.model = Some(m.to_string());
                fast_offset += 1;
                assigned += 1;
            }
        } else if let Some(m) = pools.nth_strong(strong_offset) {
            node.model = Some(m.to_string());
            strong_offset += 1;
            assigned += 1;
        }
    }

    if assigned > 0 {
        // #1901 Layer 4: log the lane→model pools alongside the counts, so a
        // later "no provider registered for key '…'" resolution failure can
        // be traced back to the lane (strong/fast) that produced the model
        // in one grep instead of re-deriving the assignment by hand.
        tracing::info!(
            assigned,
            skipped_explicit,
            strong_pool_size = pools.strong.len(),
            fast_pool_size = pools.fast.len(),
            strong_pool = ?pools.strong,
            fast_pool = ?pools.fast,
            "model_assignment: applied defaults to {} pipeline node attribute(s)",
            assigned
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{HandlerKind, PipelineGraph, PipelineNode};
    use std::collections::HashMap;

    fn n(id: &str, handler: HandlerKind) -> (String, PipelineNode) {
        (
            id.to_string(),
            PipelineNode {
                id: id.to_string(),
                handler,
                ..Default::default()
            },
        )
    }

    fn graph_with(nodes: Vec<(String, PipelineNode)>) -> PipelineGraph {
        PipelineGraph {
            id: "test".into(),
            label: None,
            default_model: None,
            max_total_tokens: None,
            default_timeout_secs: None,
            result_fidelity: None,
            nodes: nodes.into_iter().collect::<HashMap<_, _>>(),
            edges: Vec::new(),
            subgraphs: Vec::new(),
        }
    }

    fn pools(strong: &[&str], fast: &[&str]) -> ModelPoolsArg {
        ModelPoolsArg {
            strong: strong.iter().map(|s| s.to_string()).collect(),
            fast: fast.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn dynamic_parallel_gets_full_fast_pool_and_strong_planner() {
        let mut g = graph_with(vec![n("plan_and_search", HandlerKind::DynamicParallel)]);
        assign_with_pools_for_test(&mut g, pools(&["strong-a"], &["fast-a", "fast-b"]));
        let node = &g.nodes["plan_and_search"];
        assert_eq!(node.model.as_deref(), Some("fast-a,fast-b"));
        assert_eq!(node.planner_model.as_deref(), Some("strong-a"));
    }

    #[test]
    fn search_id_node_gets_fast_synthesis_gets_strong() {
        let mut g = graph_with(vec![
            n("search_news", HandlerKind::Codergen),
            n("synthesize", HandlerKind::Codergen),
        ]);
        assign_with_pools_for_test(&mut g, pools(&["claude-strong"], &["minimax-fast"]));
        assert_eq!(
            g.nodes["search_news"].model.as_deref(),
            Some("minimax-fast")
        );
        assert_eq!(
            g.nodes["synthesize"].model.as_deref(),
            Some("claude-strong")
        );
    }

    #[test]
    fn assign_fast_handles_broader_retrieval_id_aliases() {
        // Codex review #1.3: the FAST heuristic was historically only
        // triggered by `search` / `retrieve` substrings, so operator
        // pipelines that use natural verbs (`gather_news`, `crawl_pages`,
        // `investigate_topic`) silently fell through to STRONG. With the
        // broadened substring list, all three retrieval nodes should land
        // on the FAST pool while the synthesis node still gets STRONG.
        let mut g = graph_with(vec![
            n("gather_news", HandlerKind::Codergen),
            n("crawl_pages", HandlerKind::Codergen),
            n("investigate_topic", HandlerKind::Codergen),
            n("synthesize_report", HandlerKind::Codergen),
        ]);
        assign_with_pools_for_test(
            &mut g,
            pools(&["claude-strong"], &["fast-a", "fast-b", "fast-c"]),
        );
        for id in &["gather_news", "crawl_pages", "investigate_topic"] {
            let assigned = g.nodes[*id].model.as_deref().unwrap_or("");
            assert!(
                assigned.starts_with("fast-"),
                "retrieval node `{id}` should be FAST, got `{assigned}`"
            );
        }
        assert_eq!(
            g.nodes["synthesize_report"].model.as_deref(),
            Some("claude-strong")
        );
    }

    #[test]
    fn explicit_model_is_preserved_never_overwritten() {
        let mut g = graph_with(vec![(
            "analyze".into(),
            PipelineNode {
                id: "analyze".into(),
                handler: HandlerKind::Codergen,
                model: Some("operator-pinned".into()),
                ..Default::default()
            },
        )]);
        assign_with_pools_for_test(&mut g, pools(&["strong-a"], &["fast-a"]));
        assert_eq!(g.nodes["analyze"].model.as_deref(), Some("operator-pinned"));
    }

    #[test]
    fn non_llm_handlers_get_no_model() {
        let mut g = graph_with(vec![
            n("guard", HandlerKind::Gate),
            n("touch", HandlerKind::Shell),
            n("passthrough", HandlerKind::Noop),
        ]);
        assign_with_pools_for_test(&mut g, pools(&["strong-a"], &["fast-a"]));
        for id in &["guard", "touch", "passthrough"] {
            assert!(
                g.nodes[*id].model.is_none(),
                "non-llm handler `{id}` should not be assigned a model"
            );
        }
    }

    #[test]
    fn empty_pools_are_a_no_op() {
        let mut g = graph_with(vec![n("synth", HandlerKind::Codergen)]);
        assign_with_pools_for_test(&mut g, pools(&[], &[]));
        assert!(g.nodes["synth"].model.is_none());
    }

    #[test]
    fn build_pools_filters_unstable_models() {
        let catalog = ModelCatalog {
            models: vec![
                CatalogEntry {
                    provider: "x/healthy-strong".into(),
                    model_type: "strong".into(),
                    stability: 0.99,
                    score: 0.10,
                    cost_out: 0.0,
                },
                CatalogEntry {
                    provider: "x/broken-strong".into(),
                    model_type: "strong".into(),
                    stability: 0.10,
                    score: 0.01, // best score, but unstable
                    cost_out: 0.0,
                },
                CatalogEntry {
                    provider: "y/fast-a".into(),
                    model_type: "fast".into(),
                    stability: 0.95,
                    score: 0.15,
                    cost_out: 0.0,
                },
            ],
        };
        let pools = build_pools(&catalog, None).expect("non-empty");
        // The unstable strong should have been filtered out — only
        // healthy-strong survives.
        assert_eq!(pools.strong, vec!["healthy-strong".to_string()]);
        assert_eq!(pools.fast, vec!["fast-a".to_string()]);
    }

    /// Minimal lane provider for router-backed tests: `resolve()` only
    /// clones Arcs, `chat` is never reached.
    struct LaneStub(&'static str);

    #[async_trait::async_trait]
    impl octos_llm::LlmProvider for LaneStub {
        async fn chat(
            &self,
            _messages: &[octos_core::Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> eyre::Result<octos_llm::ChatResponse> {
            unreachable!("assignment must not call chat()")
        }

        async fn chat_stream(
            &self,
            _messages: &[octos_core::Message],
            _tools: &[octos_llm::ToolSpec],
            _config: &octos_llm::ChatConfig,
        ) -> eyre::Result<octos_llm::ChatStream> {
            unreachable!("assignment must not call chat_stream()")
        }

        fn model_id(&self) -> &str {
            self.0
        }

        fn provider_name(&self) -> &str {
            self.0
        }
    }

    /// A router registering only the given lane keys, mirroring a profile
    /// whose research lanes are `cheap`/`strong` but nothing else.
    fn lane_router(keys: &[&str]) -> std::sync::Arc<ProviderRouter> {
        use std::sync::Arc;
        let router = Arc::new(ProviderRouter::new());
        for key in keys {
            router.register(key, Arc::new(LaneStub("lane")));
        }
        router
    }

    fn write_catalog(dir: &Path, models_json: &str) {
        std::fs::write(
            dir.join("model_catalog.json"),
            format!(r#"{{"models":[{models_json}]}}"#),
        )
        .unwrap();
    }

    /// #1901 Layer 1: the persisted `model_catalog.json` is a deliberate
    /// superset, so a profile whose router registers only lane keys
    /// (`cheap`/`strong`) must not have those catalog models assigned to
    /// nodes — every assignment would be unresolvable at execution time and
    /// silently run on the default provider instead. Unset nodes stay unset
    /// (the documented no-catalog behavior); explicit models keep winning.
    #[test]
    fn unregistered_catalog_models_are_never_assigned() {
        let dir = tempfile::tempdir().unwrap();
        write_catalog(
            dir.path(),
            concat!(
                r#"{"provider":"x/qwen3-max","type":"strong","stability":0.99,"score":0.10},"#,
                r#"{"provider":"y/kimi-fast","type":"fast","stability":0.95,"score":0.15}"#,
            ),
        );
        let router = lane_router(&["cheap", "strong"]);

        let mut g = graph_with(vec![
            n("plan_and_search", HandlerKind::DynamicParallel),
            n("search_news", HandlerKind::Codergen),
            (
                "analyze".into(),
                PipelineNode {
                    id: "analyze".into(),
                    handler: HandlerKind::Codergen,
                    model: Some("operator-pinned".into()),
                    ..Default::default()
                },
            ),
            n("synthesize", HandlerKind::Codergen),
        ]);
        assign_from_catalog_dir(&mut g, dir.path(), Some(&router));

        for id in ["plan_and_search", "search_news", "synthesize"] {
            assert!(
                g.nodes[id].model.is_none(),
                "node `{id}` must not be assigned an unregistered catalog model"
            );
        }
        assert_eq!(g.nodes["analyze"].model.as_deref(), Some("operator-pinned"));
        assert!(
            g.nodes["plan_and_search"].planner_model.is_none(),
            "the strong pool was entirely unregistered, so no planner model either"
        );
    }

    /// The filter uses the router's own resolution semantics (exact →
    /// case-insensitive → prefix split → suffix alias), NOT `keys()`
    /// membership — narrowing to `keys()` was the #1904 defect, because the
    /// router resolves lane keys and aliases a literal membership check
    /// rejects. `moonshotai/kimi-k2.5` registered as a compound key keeps a
    /// `moonshotai/kimi-k2.5` catalog entry assignable via the suffix-alias
    /// arm, while `qwen3-max` (resolvable by nothing) is skipped.
    #[test]
    fn router_filter_uses_resolution_semantics_not_key_membership() {
        let dir = tempfile::tempdir().unwrap();
        write_catalog(
            dir.path(),
            concat!(
                r#"{"provider":"x/qwen3-max","type":"strong","stability":0.99,"score":0.05},"#,
                r#"{"provider":"moonshotai/kimi-k2.5","type":"strong","stability":0.98,"score":0.10}"#,
            ),
        );
        let router = lane_router(&["moonshotai/kimi-k2.5", "cheap"]);

        let mut g = graph_with(vec![n("synthesize", HandlerKind::Codergen)]);
        assign_from_catalog_dir(&mut g, dir.path(), Some(&router));

        assert_eq!(
            g.nodes["synthesize"].model.as_deref(),
            Some("kimi-k2.5"),
            "the resolvable catalog model must be assigned (via the router's \
             suffix-alias arm), not skipped for not being a literal router key"
        );
    }

    /// Fails open: with no router in play, assignment keeps using the
    /// catalog as-is (#1902's tolerance semantics — a router-less profile
    /// must not lose strong/fast defaults).
    #[test]
    fn absent_router_keeps_catalog_assignment_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        write_catalog(
            dir.path(),
            r#"{"provider":"x/qwen3-max","type":"strong","stability":0.99,"score":0.10}"#,
        );

        let mut g = graph_with(vec![n("synthesize", HandlerKind::Codergen)]);
        assign_from_catalog_dir(&mut g, dir.path(), None);

        assert_eq!(
            g.nodes["synthesize"].model.as_deref(),
            Some("qwen3-max"),
            "no router ⇒ no new strictness; the catalog pool still fills unset nodes"
        );
    }

    /// The DynamicParallel `model=` attribute is a comma-joined POOL, so the
    /// filter must apply per entry: only resolvable fast-lane models may end
    /// up in the joined string (every entry is round-robined onto a worker
    /// that later resolves it individually).
    #[test]
    fn dynamic_parallel_pool_joins_only_resolvable_entries() {
        let dir = tempfile::tempdir().unwrap();
        write_catalog(
            dir.path(),
            concat!(
                r#"{"provider":"x/qwen3-max","type":"strong","stability":0.99,"score":0.10},"#,
                r#"{"provider":"y/kimi-fast","type":"fast","stability":0.95,"score":0.15},"#,
                r#"{"provider":"z/gpt-4o-mini","type":"fast","stability":0.97,"score":0.20}"#,
            ),
        );
        let router = lane_router(&["cheap", "strong", "kimi-fast"]);

        let mut g = graph_with(vec![n("plan_and_search", HandlerKind::DynamicParallel)]);
        assign_from_catalog_dir(&mut g, dir.path(), Some(&router));

        // qwen3-max (strong pool) and gpt-4o-mini (fast pool) are
        // unregistered → skipped; kimi-fast survives. The strong pool is
        // empty, so it aliases to the surviving fast pool.
        assert_eq!(
            g.nodes["plan_and_search"].model.as_deref(),
            Some("kimi-fast"),
            "only router-resolvable entries may appear in the worker pool"
        );
        assert_eq!(
            g.nodes["plan_and_search"].planner_model.as_deref(),
            Some("kimi-fast"),
            "empty strong pool aliases to the surviving fast pool (existing behavior)"
        );
    }
}
