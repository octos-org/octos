use std::path::Path;
use std::sync::Arc;

use octos_llm::{
    AdaptiveConfig, AdaptiveMode, AdaptiveRouter, BaselineEntry, ContextWindowOverride,
    LlmProvider, ModelCatalogEntry, ProviderChain, QosCatalog, RetryProvider,
};
use tracing::{info, warn};

use crate::commands::chat::create_provider_with_api_type;
use crate::config::Config;

/// #2142: wrap a freshly-created (probe-wrapped) provider in the operator's
/// `context_window` override when the config sets one.
///
/// The override sits just OUTSIDE the local-context probe (#2135) and INSIDE
/// `RetryProvider` / `ProviderChain` / `AdaptiveRouter` — all of which delegate
/// `context_window()` as of #2135 — so the operator's value resolves through
/// the entire runtime stack and beats BOTH the static catalog and the runtime
/// probe. Applied per provider (primary and each fallback independently) so a
/// primary pin never leaks onto a fallback's own window; the router then
/// aggregates the per-slot values as it already does. `None` leaves the
/// provider untouched.
pub(crate) fn apply_context_window_override(
    provider: Arc<dyn LlmProvider>,
    window: Option<u32>,
    slot: &str,
) -> Arc<dyn LlmProvider> {
    match window {
        Some(w) => {
            info!(
                context_window = w,
                slot,
                "context window overridden by config.llm (operator override wins over probe/catalog)"
            );
            Arc::new(ContextWindowOverride::new(provider, w))
        }
        None => provider,
    }
}

/// The canonical model catalog (`model_catalog.json`), compiled in. This is the
/// single source of truth for model provisioning and the researched
/// context-window / pricing floor. It ships next to the binary at release time
/// (see `scripts/build-local-bundle.sh`), but is also embedded so a fresh
/// install — one with no per-profile data-dir catalog and no `~/.octos`
/// catalog yet — still seeds the adaptive router, the context-window table, and
/// the pricing table with researched values instead of cold-start zeros.
///
/// `crates/octos-cli/src/api/ui_protocol.rs` (onboarding) and
/// `crates/octos-cli/src/commands/init.rs` reference this same const so there is
/// exactly one embedded copy.
pub(crate) const EMBEDDED_MODEL_CATALOG: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../model_catalog.json"
));

/// Parse the compiled-in canonical catalog. `None` only if the committed file
/// is malformed (a build-time invariant, so effectively always `Some`).
pub(crate) fn embedded_qos_catalog() -> Option<QosCatalog> {
    serde_json::from_str(EMBEDDED_MODEL_CATALOG).ok()
}

/// Merge a live-scored `overlay` catalog onto a full canonical `base` catalog.
///
/// For a provider present in BOTH, the merged entry keeps the canonical
/// **static** metadata (cost, context window, max output, model type,
/// deep-search quality `ds_output`) from the base and takes only the
/// **dynamic** live QoS (score, stability, latency) from the overlay. This is
/// deliberate: the router export's static fields are whatever was seeded from
/// the previous on-disk catalog, so a plain overlay-wins merge would let a stale
/// cost/context value written before an upgrade win over the corrected canonical
/// value and re-persist itself forever — the on-disk catalog would never
/// converge to the SSOT. `ds_output` (deep-search quality) is also static/
/// seed-only, but with one twist: the canonical catalog uses `0` as a "not
/// evaluated" sentinel, so it wins only when it carries a positive evaluated
/// value; when canonical `ds_output` is 0 the overlay's value is preserved so an
/// older on-disk benchmark is not erased. (Contrast `cost`, where 0 is a real
/// free-tier price and canonical 0 must win.) Overlay-only providers (e.g. a
/// configured custom-`base_url` model absent from the canonical catalog) are
/// kept verbatim; base-only providers are preserved.
///
/// The exporter persists `merge(embedded_base, router_export)` so the on-disk
/// `model_catalog.json` stays a full superset (all researched entries + live
/// scores for configured lanes) and never shrinks to just the configured lanes.
/// Output is sorted by provider for deterministic diffs.
/// Strip an OpenAI-compatible `@host` tag from the family segment of a provider
/// key (`moonshot@api/kimi-k2.5` -> `moonshot/kimi-k2.5`). Returns `None` when
/// there is no tag, so callers only do the extra lookup when it can differ.
fn normalized_provider_key(provider: &str) -> Option<String> {
    let (family, model) = provider.split_once('/')?;
    let bare = family.split('@').next().unwrap_or(family);
    (bare != family).then(|| format!("{bare}/{model}"))
}

pub(crate) fn merge_qos_catalog(base: &QosCatalog, overlay: &QosCatalog) -> QosCatalog {
    // Immutable view of the canonical base, keyed by provider. Static-field
    // lookups below consult ONLY this map — never the accumulating `by_provider`
    // — so an overlay lane can never source its "canonical" static metadata from
    // a prior overlay lane that merely shares a host-tag-normalized key (e.g. an
    // overlay-only `openai/custom` followed by `openai@proxy/custom`: both are
    // custom lanes absent from the base and must be kept verbatim, not merged
    // into each other).
    let base_by_provider: std::collections::HashMap<&str, &ModelCatalogEntry> = base
        .models
        .iter()
        .map(|entry| (entry.provider.as_str(), entry))
        .collect();
    let mut by_provider: std::collections::BTreeMap<String, ModelCatalogEntry> = base
        .models
        .iter()
        .map(|entry| (entry.provider.clone(), entry.clone()))
        .collect();
    for entry in &overlay.models {
        // Find the canonical base entry by the exact key, then by the
        // host-tag-stripped key — a live OpenAI-compatible lane exports a
        // `moonshot@api/kimi-k2.5` key that must still reconcile with the
        // untagged canonical `moonshot/kimi-k2.5` so its corrected static
        // metadata wins. Copy the static fields out (all `Copy`) so the
        // immutable borrow ends before the insert below.
        let base_static = base_by_provider
            .get(entry.provider.as_str())
            .copied()
            .or_else(|| {
                normalized_provider_key(&entry.provider)
                    .and_then(|key| base_by_provider.get(key.as_str()).copied())
            })
            .map(|b| {
                (
                    b.model_type,
                    b.cost_in,
                    b.cost_out,
                    b.context_window,
                    b.max_output,
                    b.ds_output,
                    b.is_family_default,
                )
            });
        let merged = match base_static {
            Some((
                model_type,
                cost_in,
                cost_out,
                context_window,
                max_output,
                ds_output,
                is_family_default,
            )) => {
                ModelCatalogEntry {
                    provider: entry.provider.clone(),
                    // Static — canonical base wins (SSOT convergence). The
                    // family-default flag is canonical-only: a live QoS row must
                    // never be able to elect a different default model.
                    is_family_default,
                    model_type,
                    cost_in,
                    cost_out,
                    context_window,
                    max_output,
                    // `ds_output` (deep-search quality) is static/seed-only, BUT
                    // the canonical catalog leaves it `0` = "not evaluated". Taking
                    // a canonical 0 unconditionally would erase a positive value
                    // an older on-disk catalog had actually benchmarked. So the
                    // canonical wins only when it carries an evaluated value;
                    // otherwise the overlay's value is preserved. (`cost` differs:
                    // 0 there is a real free-tier price, so canonical 0 must win.)
                    ds_output: if ds_output != 0 {
                        ds_output
                    } else {
                        entry.ds_output
                    },
                    // Dynamic — live overlay wins.
                    stability: entry.stability,
                    tool_avg_ms: entry.tool_avg_ms,
                    p95_ms: entry.p95_ms,
                    score: entry.score,
                }
            }
            None => entry.clone(),
        };
        by_provider.insert(entry.provider.clone(), merged);
    }
    QosCatalog {
        // The overlay carries the fresh export timestamp; prefer it so the file
        // reflects when the live scores were last written.
        updated_at: overlay.updated_at.clone(),
        models: by_provider.into_values().collect(),
    }
}

/// Result of wiring up the LLM provider chain together with full
/// QoS-aware adaptive routing.
///
/// `llm` is the top-level provider that callers should pass to
/// `Agent`/`SessionManager`. `adaptive_router` is `Some` only when more
/// than one provider was successfully built — gateway uses this typed
/// handle later (for `ActorFactory::adaptive_router` and the periodic
/// metrics exporter). `runtime_qos_catalog` is the catalog that was
/// (a) materialized from the live router export when available, or
/// (b) derived from the cold-start seed otherwise; it has already been
/// pushed into `octos_llm::context` and `octos_llm::pricing` and
/// persisted to `model_catalog.json` before this struct is returned.
pub(crate) struct AdaptiveProviderBundle {
    pub llm: Arc<dyn LlmProvider>,
    pub adaptive_router: Option<Arc<AdaptiveRouter>>,
    pub runtime_qos_catalog: Option<QosCatalog>,
}

/// Whether [`build_adaptive_provider_chain`] should spawn the periodic
/// `model_catalog.json` exporter. Production callers want `Spawn`; tests
/// want `Disabled` to avoid leaking tokio tasks past the test scope.
///
/// Typed so production call sites can't accidentally pass `false` — the
/// 30s exporter is what keeps the persisted catalog in lockstep with
/// the running router's lane scores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExporterMode {
    Spawn,
    /// Test-only — keeps the helper from leaking a tokio task past
    /// the test scope. Allow dead_code in production builds where
    /// only `Spawn` is ever constructed.
    #[allow(dead_code)]
    Disabled,
}

/// Build the LLM provider chain with full QoS adaptive wiring.
///
/// Mirrors what `gateway_runtime.rs` used to do inline so that
/// `octos serve` can stay in lockstep with `octos gateway`:
///
/// 1. Wraps the primary `base_provider` in `RetryProvider` (unless
///    `no_retry`), layers in each `config.fallback_models` entry on
///    top, propagating each fallback's `cost_per_m` into the cost
///    vector and its `api_key_env` into the per-fallback config clone.
/// 2. When more than one provider exists, builds an `AdaptiveRouter`
///    with `.with_adaptive_config(mode, qos)` derived from
///    `config.adaptive_routing`. Otherwise falls back to
///    `ProviderChain` (or the bare `RetryProvider` when no fallbacks).
/// 3. Loads `provider_baseline.json` from `data_dir` first, then
///    `~/.octos/`. Seeds the router with the parsed entries. Logs an
///    info line either way.
/// 4. Seeds the router with the model catalog from
///    `load_seed_qos_catalog`.
/// 5. Materializes the runtime QoS catalog (preferring the live
///    router export over the cold-start seed) and seeds
///    `octos_llm::context::seed_from_catalog` +
///    `octos_llm::pricing::seed_pricing_catalog`.
/// 6. Persists `model_catalog.json` next to `data_dir`.
/// 7. When `exporter == ExporterMode::Spawn` and an `AdaptiveRouter`
///    exists, spawns a tokio task that re-writes `model_catalog.json`
///    every 30s from the router's live export. Tests should pass
///    `ExporterMode::Disabled` to keep the test free of leaked tokio
///    tasks.
pub(crate) fn build_adaptive_provider_chain(
    base_provider: Arc<dyn LlmProvider>,
    config: &Config,
    data_dir: &Path,
    no_retry: bool,
    exporter: ExporterMode,
) -> AdaptiveProviderBundle {
    let mut adaptive_router_ref: Option<Arc<AdaptiveRouter>> = None;

    // #2142: operator override of the primary's effective context window,
    // applied before RetryProvider/router wrap it so it propagates through
    // the delegating stack and beats the probe/catalog.
    let base_provider =
        apply_context_window_override(base_provider, config.context_window, "primary");

    let llm: Arc<dyn LlmProvider> = if no_retry {
        base_provider
    } else if config.fallback_models.is_empty() {
        Arc::new(RetryProvider::new(base_provider))
    } else {
        let mut providers: Vec<Arc<dyn LlmProvider>> =
            vec![Arc::new(RetryProvider::new(base_provider))];
        let mut costs: Vec<f64> = vec![0.0]; // primary cost unknown
        for fb in &config.fallback_models {
            // Always swap in this fallback's own `api_key_env`. When the
            // fallback omits it (None), we clear the primary's value so
            // `Config::get_api_key` falls back to the provider registry
            // default for the fallback's family — otherwise a
            // cross-provider fallback (e.g. deepseek behind moonshot)
            // would inherit the primary's AUTODL_API_KEY instead of
            // using DEEPSEEK_API_KEY.
            let mut fb_config = config.clone();
            fb_config.api_key_env = fb.api_key_env.clone();
            match create_provider_with_api_type(
                &fb.provider,
                &fb_config,
                fb.model.clone(),
                fb.base_url.clone(),
                fb.api_type.as_deref(),
            ) {
                Ok(p) => {
                    // #2142: per-fallback context-window override.
                    let p = apply_context_window_override(p, fb.context_window, "fallback");
                    providers.push(Arc::new(RetryProvider::new(p)));
                    costs.push(fb.cost_per_m.unwrap_or(0.0));
                }
                Err(e) => {
                    warn!(provider = %fb.provider, error = %e, "skipping fallback provider");
                }
            }
        }
        // Adaptive routing must be *opt-in*: when `adaptive_routing` is
        // absent or `enabled = false`, fall back to the plain static
        // `ProviderChain`. This kills the silent default-ON behavior the
        // previous implementation had (router wrapping always when
        // `providers.len() > 1`).
        let adaptive_enabled = config
            .adaptive_routing
            .as_ref()
            .map(|c| c.enabled)
            .unwrap_or(false);
        if providers.len() > 1 && adaptive_enabled {
            let ar_config = config
                .adaptive_routing
                .as_ref()
                .expect("adaptive_enabled implies adaptive_routing.is_some()");
            let adaptive_config = AdaptiveConfig::from(ar_config);
            info!(
                "adaptive routing enabled ({} providers, mode={:?}, qos={})",
                providers.len(),
                ar_config.mode,
                ar_config.qos_ranking
            );
            let mode: AdaptiveMode = ar_config.mode.into();
            let qos = ar_config.qos_ranking;
            let router = Arc::new(
                AdaptiveRouter::new(providers, &costs, adaptive_config)
                    .with_adaptive_config(mode, qos),
            );
            // Wave-4c: surface AutoEscalationConfig from config.json so
            // operators can disable the latency feedback loop (e.g. CI,
            // benchmarks). The router defaults to enabled; this only
            // overrides when an `adaptive_routing.auto_escalation` block
            // exists. (Merge note: ar_config is already &AdaptiveRoutingConfig
            // here — the outer `if adaptive_enabled` ensures Some.)
            router.set_auto_escalation_config(octos_llm::AutoEscalationConfig::from(
                &ar_config.auto_escalation,
            ));
            adaptive_router_ref = Some(router.clone());
            router
        } else {
            if providers.len() > 1 {
                info!(
                    "adaptive routing disabled (enabled=false or omitted) — \
                     falling back to static ProviderChain ({} providers)",
                    providers.len()
                );
            }
            Arc::new(ProviderChain::new(providers))
        }
    };

    let catalog_path = data_dir.join("model_catalog.json");
    let qos_scoring_config = config
        .adaptive_routing
        .as_ref()
        .map(AdaptiveConfig::from)
        .unwrap_or_default();
    let qos_ranking_enabled = config
        .adaptive_routing
        .as_ref()
        .map(|cfg| cfg.qos_ranking)
        .unwrap_or(true);
    // Merge the canonical STATIC metadata onto the on-disk seed BEFORE it seeds
    // the router and the runtime context/pricing tables. Otherwise a stale static
    // value in an on-disk `model_catalog.json` written by a pre-upgrade build (or
    // hand-edited) would drive THIS process's routing scores, cost estimates, and
    // context windows until the next restart: the persisted file is corrected
    // below via the same merge, but the already-seeded runtime tables would keep
    // the stale values. `embedded_base` is computed once and reused as the
    // persist base further down.
    let embedded_base = embedded_qos_catalog();
    let seed_catalog = load_seed_qos_catalog(data_dir).map(|on_disk| match &embedded_base {
        Some(base) => merge_qos_catalog(base, &on_disk),
        None => on_disk,
    });

    let runtime_qos_catalog: Option<QosCatalog> = if let Some(ref router) = adaptive_router_ref {
        // Look in data_dir first, then fall back to ~/.octos/ (shared across profiles)
        let baseline_candidates = [
            data_dir.join("provider_baseline.json"),
            dirs::home_dir()
                .unwrap_or_default()
                .join(".octos/provider_baseline.json"),
        ];
        let mut baseline_loaded = false;
        for baseline_path in &baseline_candidates {
            if let Ok(json) = std::fs::read_to_string(baseline_path) {
                match serde_json::from_str::<Vec<BaselineEntry>>(&json) {
                    Ok(entries) => {
                        router.seed_baseline(&entries);
                        info!(
                            path = %baseline_path.display(),
                            entries = entries.len(),
                            "loaded provider baseline"
                        );
                        baseline_loaded = true;
                        break;
                    }
                    Err(e) => {
                        warn!(error = %e, path = %baseline_path.display(), "failed to parse provider_baseline.json")
                    }
                }
            }
        }
        if !baseline_loaded {
            info!("no provider_baseline.json found, using cold-start scoring");
        }

        if let Some(ref catalog) = seed_catalog {
            router.seed_catalog(&catalog.models);
            info!(models = catalog.models.len(), "loaded model catalog");
        }

        materialize_runtime_qos_catalog(
            seed_catalog.as_ref(),
            Some(router.export_model_catalog()),
            &qos_scoring_config,
            qos_ranking_enabled,
        )
    } else {
        materialize_runtime_qos_catalog(
            seed_catalog.as_ref(),
            None,
            &qos_scoring_config,
            qos_ranking_enabled,
        )
    };

    // The persisted `model_catalog.json` merges the sparse live export ON TOP of
    // the full compiled-in canonical catalog, so it stays a complete superset
    // (all researched entries + live scores for configured lanes) rather than
    // shrinking to just the configured lanes. This also "seeds the data-dir on
    // first run": a fresh install writes the full catalog here immediately. Reuse
    // the base already loaded above for the seed merge (single embed parse).
    let persist_base = embedded_base;

    if let Some(ref catalog) = runtime_qos_catalog {
        let ctx_entries: Vec<(String, u64, u64)> = catalog
            .models
            .iter()
            .map(|m| (m.provider.clone(), m.context_window, m.max_output))
            .collect();
        octos_llm::context::seed_from_catalog(&ctx_entries);
        let price_entries: Vec<(String, f64, f64)> = catalog
            .models
            .iter()
            .map(|m| (m.provider.clone(), m.cost_in, m.cost_out))
            .collect();
        octos_llm::pricing::seed_pricing_catalog(&price_entries);
        let to_persist = match &persist_base {
            Some(base) => merge_qos_catalog(base, catalog),
            None => catalog.clone(),
        };
        persist_qos_catalog(&catalog_path, &to_persist);
    }

    if exporter == ExporterMode::Spawn {
        if let Some(ref router) = adaptive_router_ref {
            let metrics_router = router.clone();
            let exporter_path = catalog_path.clone();
            // The periodic exporter merges each fresh export onto the same full
            // base, so the 30s rewrite never shrinks the on-disk catalog.
            let exporter_base = persist_base.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                loop {
                    interval.tick().await;
                    let export = metrics_router.export_model_catalog();
                    let to_write = match &exporter_base {
                        Some(base) => merge_qos_catalog(base, &export),
                        None => export,
                    };
                    if let Ok(json) = serde_json::to_string_pretty(&to_write) {
                        let _ = tokio::fs::write(&exporter_path, &json).await;
                    }
                }
            });
        }
    }

    AdaptiveProviderBundle {
        llm,
        adaptive_router: adaptive_router_ref,
        runtime_qos_catalog,
    }
}

/// Derive a runtime QoS catalog from static model metadata when no adaptive
/// router is active.
pub(crate) fn derive_cold_start_qos_catalog(
    entries: &[ModelCatalogEntry],
    config: &AdaptiveConfig,
    qos_ranking: bool,
) -> QosCatalog {
    octos_llm::derive_cold_start_catalog(entries, config, qos_ranking)
}

pub(crate) fn load_seed_qos_catalog(data_dir: &Path) -> Option<QosCatalog> {
    let candidates = [
        data_dir.join("model_catalog.json"),
        dirs::home_dir()
            .unwrap_or_default()
            .join(".octos/model_catalog.json"),
    ];
    for path in &candidates {
        if let Ok(json) = std::fs::read_to_string(path) {
            if let Ok(catalog) = serde_json::from_str::<QosCatalog>(&json) {
                return Some(catalog);
            }
        }
    }
    // Fresh install: no runtime catalog on disk yet. Fall back to the compiled-in
    // canonical catalog so the router / context-window table / pricing table are
    // seeded with researched values instead of cold-start zeros. (On any machine
    // that already has a runtime catalog this branch is never reached.)
    embedded_qos_catalog()
}

pub(crate) fn persist_qos_catalog(path: &Path, catalog: &QosCatalog) {
    match serde_json::to_string_pretty(catalog) {
        Ok(json) => {
            if let Err(error) = std::fs::write(path, json) {
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "failed to persist runtime model catalog"
                );
            }
        }
        Err(error) => tracing::warn!(
            path = %path.display(),
            %error,
            "failed to serialize runtime model catalog"
        ),
    }
}

pub(crate) fn materialize_runtime_qos_catalog(
    seed_catalog: Option<&QosCatalog>,
    adaptive_export: Option<QosCatalog>,
    config: &AdaptiveConfig,
    qos_ranking: bool,
) -> Option<QosCatalog> {
    adaptive_export.or_else(|| {
        seed_catalog
            .map(|catalog| derive_cold_start_qos_catalog(&catalog.models, config, qos_ranking))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_llm::ModelType;
    use tempfile::tempdir;

    fn sample_catalog(scores: [f64; 2]) -> QosCatalog {
        QosCatalog {
            updated_at: "2026-04-11T00:00:00Z".to_string(),
            models: vec![
                ModelCatalogEntry {
                    provider: "zai/glm-5-turbo".to_string(),
                    model_type: ModelType::Fast,
                    is_family_default: false,
                    stability: 0.97,
                    tool_avg_ms: 900,
                    p95_ms: 1500,
                    score: scores[0],
                    cost_in: 0.5,
                    cost_out: 2.0,
                    ds_output: 1200,
                    context_window: 128_000,
                    max_output: 8_192,
                },
                ModelCatalogEntry {
                    provider: "dashscope/qwen3.5-plus".to_string(),
                    model_type: ModelType::Strong,
                    is_family_default: false,
                    stability: 0.92,
                    tool_avg_ms: 1400,
                    p95_ms: 2400,
                    score: scores[1],
                    cost_in: 0.8,
                    cost_out: 3.2,
                    ds_output: 800,
                    context_window: 128_000,
                    max_output: 16_384,
                },
            ],
        }
    }

    #[test]
    fn load_seed_qos_catalog_reads_profile_local_catalog() {
        let temp = tempdir().unwrap();
        let data_dir = temp.path().join("profile-data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let path = data_dir.join("model_catalog.json");
        let catalog = sample_catalog([0.0, 0.0]);
        std::fs::write(&path, serde_json::to_string_pretty(&catalog).unwrap()).unwrap();

        let loaded = load_seed_qos_catalog(&data_dir).expect("catalog should load");
        assert_eq!(loaded.models.len(), 2);
        assert_eq!(loaded.models[0].provider, "zai/glm-5-turbo");
        assert_eq!(loaded.models[1].provider, "dashscope/qwen3.5-plus");
    }

    fn scored_entry(provider: &str, score: f64, ctx: u64) -> ModelCatalogEntry {
        ModelCatalogEntry {
            provider: provider.to_string(),
            model_type: ModelType::Strong,
            is_family_default: false,
            stability: 1.0,
            tool_avg_ms: 0,
            p95_ms: 0,
            score,
            cost_in: 0.0,
            cost_out: 0.0,
            ds_output: 0,
            context_window: ctx,
            max_output: 0,
        }
    }

    /// The exporter persists `merge(embedded_base, live_export)`: every base
    /// entry survives; for an overlapping provider the canonical STATIC metadata
    /// (context window etc.) wins while the live DYNAMIC score is taken from the
    /// overlay; a live-only lane is appended. This keeps the on-disk catalog a
    /// full superset that converges to the SSOT instead of re-persisting stale
    /// static values written before an upgrade.
    #[test]
    fn merge_qos_catalog_preserves_base_and_overlays_live() {
        // Base deepseek carries an EVALUATED canonical deep-search quality (>0);
        // the overlay's is stale, so the canonical value wins. Base glm-5.3 leaves
        // ds_output at the `0` "not evaluated" sentinel (via `scored_entry`), so an
        // evaluated overlay value there must be PRESERVED, not clobbered by 0.
        let mut base_deepseek = scored_entry("deepseek/deepseek-v4-pro", 0.0, 1_048_576);
        base_deepseek.ds_output = 1500;
        let base = QosCatalog {
            updated_at: "SEED".to_string(),
            models: vec![scored_entry("zai/glm-5.3", 0.0, 1_000_000), base_deepseek],
        };
        let mut overlay_deepseek = scored_entry("deepseek/deepseek-v4-pro", 0.87, 999);
        overlay_deepseek.ds_output = 1; // stale seed re-exported by the router
        // Host-tagged lane whose bare base (`zai/glm-5.3`) is unevaluated (0);
        // this lane carries a real benchmarked ds_output the merge must keep.
        let mut overlay_glm = scored_entry("zai@api/glm-5.3", 0.9, 111);
        overlay_glm.ds_output = 2222;
        let overlay = QosCatalog {
            updated_at: "2026-07-12T00:00:00Z".to_string(),
            models: vec![
                // Live score for a configured lane already in the base, but with
                // a STALE context window + ds_output (as if seeded from a
                // pre-upgrade catalog) — the canonical values must win.
                overlay_deepseek,
                // A host-tagged OpenAI-compatible lane whose bare form IS in the
                // base — it must reconcile with `zai/glm-5.3` (stale ctx wins from
                // canonical, live score from overlay), not be treated as new.
                overlay_glm,
                // … plus a lane not in the base (custom base_url model).
                scored_entry("stub/stub-model", 0.5, 0),
                // Two overlay-only lanes that share a host-tag-normalized key but
                // are BOTH absent from the base. Ordered so the untagged lane is
                // processed first: a lookup against the accumulating merge map
                // (rather than the immutable base) would let the proxy lane copy
                // this lane's static fields. Both must be kept verbatim instead.
                scored_entry("openai/custom-model", 0.3, 40_000),
                scored_entry("openai@proxy/custom-model", 0.4, 50_000),
            ],
        };

        let merged = merge_qos_catalog(&base, &overlay);
        // Fresh export timestamp is carried onto the merged catalog.
        assert_eq!(merged.updated_at, "2026-07-12T00:00:00Z");
        // base-only (zai/glm-5.3) + overlaid (deepseek) + host-tagged
        // (zai@api/glm-5.3) + overlay-only (stub) + two custom lanes = 6.
        assert_eq!(merged.models.len(), 6);
        let by = |p: &str| merged.models.iter().find(|m| m.provider == p).unwrap();
        // Base-only entry preserved (researched context window intact).
        assert_eq!(by("zai/glm-5.3").context_window, 1_000_000);
        assert_eq!(by("zai/glm-5.3").score, 0.0);
        // Overlapping provider: live score wins (DYNAMIC) …
        assert_eq!(by("deepseek/deepseek-v4-pro").score, 0.87);
        // … but the canonical static context window wins over the stale overlay.
        assert_eq!(
            by("deepseek/deepseek-v4-pro").context_window,
            1_048_576,
            "canonical static field must win over a stale overlay value"
        );
        // ds_output: the canonical base has an EVALUATED value (>0), so it wins
        // over the stale overlay.
        assert_eq!(
            by("deepseek/deepseek-v4-pro").ds_output,
            1500,
            "evaluated canonical ds_output must win over a stale overlay value"
        );
        // Host-tagged lane reconciled with the bare canonical base: canonical
        // static context window, live overlay score.
        assert_eq!(
            by("zai@api/glm-5.3").context_window,
            1_000_000,
            "host-tagged lane converges to canonical static via key normalization"
        );
        assert_eq!(by("zai@api/glm-5.3").score, 0.9);
        // …but the canonical ds_output for glm-5.3 is the `0` "not evaluated"
        // sentinel, so the overlay's benchmarked value must be PRESERVED, not
        // erased to 0.
        assert_eq!(
            by("zai@api/glm-5.3").ds_output,
            2222,
            "an evaluated overlay ds_output survives when the canonical value is the 0 sentinel"
        );
        // Overlay-only lane appended.
        assert_eq!(by("stub/stub-model").score, 0.5);
        // Both overlay-only custom lanes are kept verbatim — the proxy lane does
        // NOT source its static metadata from the untagged lane processed before
        // it (that would happen if lookups consulted the accumulating merge map).
        assert_eq!(by("openai/custom-model").context_window, 40_000);
        assert_eq!(
            by("openai@proxy/custom-model").context_window,
            50_000,
            "overlay-only proxy lane must keep its own static, not copy a sibling overlay lane"
        );
        // Deterministic (sorted-by-provider) output.
        let providers: Vec<&str> = merged.models.iter().map(|m| m.provider.as_str()).collect();
        let mut sorted = providers.clone();
        sorted.sort_unstable();
        assert_eq!(providers, sorted);
    }

    /// The compiled-in canonical catalog (the seed floor for fresh installs) is
    /// well-formed and reflects curation: glm-5.3 + kimi-k2.6 + kimi-k3
    /// present, deepseek-chat removed.
    #[test]
    fn embedded_qos_catalog_is_curated_ssot() {
        let catalog = embedded_qos_catalog().expect("embedded canonical catalog must parse");
        let has = |p: &str| catalog.models.iter().any(|m| m.provider == p);
        assert!(has("zai/glm-5.3"), "glm-5.3 present");
        assert!(has("moonshot/kimi-k2.6"), "kimi-k2.6 present");
        assert!(has("moonshot/kimi-k3"), "kimi-k3 present");
        assert!(
            !has("deepseek/deepseek-chat"),
            "deepseek-chat curated out of the embedded catalog"
        );
        // Researched context window survives the round-trip through the embed.
        let glm52 = catalog
            .models
            .iter()
            .find(|m| m.provider == "zai/glm-5.3")
            .unwrap();
        assert_eq!(glm52.context_window, 1_000_000);
        // kimi-k3 researched values: 1M window, 131072 (default max
        // completion), official pricing $3.00/M in (cache miss) / $15.00/M out.
        let k3 = catalog
            .models
            .iter()
            .find(|m| m.provider == "moonshot/kimi-k3")
            .unwrap();
        assert_eq!(k3.context_window, 1_048_576);
        assert_eq!(k3.max_output, 131_072);
        assert!((k3.cost_in - 3.0).abs() < f64::EPSILON);
        assert!((k3.cost_out - 15.0).abs() < f64::EPSILON);
    }

    #[test]
    fn persist_qos_catalog_round_trips_runtime_scores() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("model_catalog.json");
        let catalog = sample_catalog([0.21857142857142858, 0.4]);

        persist_qos_catalog(&path, &catalog);

        let json = std::fs::read_to_string(&path).unwrap();
        let loaded: QosCatalog = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.models.len(), 2);
        assert!((loaded.models[0].score - 0.21857142857142858).abs() < 1e-12);
        assert!((loaded.models[1].score - 0.4).abs() < 1e-12);
    }

    #[test]
    fn materialize_runtime_qos_catalog_prefers_adaptive_export() {
        let seed = sample_catalog([0.0, 0.0]);
        let live = sample_catalog([0.21, 0.41]);

        let materialized = materialize_runtime_qos_catalog(
            Some(&seed),
            Some(live.clone()),
            &AdaptiveConfig::default(),
            true,
        )
        .expect("catalog should materialize");

        assert_eq!(materialized.models[0].score, live.models[0].score);
        assert_eq!(materialized.models[1].score, live.models[1].score);
    }

    #[test]
    fn materialize_runtime_qos_catalog_derives_non_zero_scores_from_seed() {
        let seed = sample_catalog([0.0, 0.0]);

        let materialized =
            materialize_runtime_qos_catalog(Some(&seed), None, &AdaptiveConfig::default(), true)
                .expect("catalog should materialize");

        assert_eq!(materialized.models.len(), seed.models.len());
        assert!(materialized.models.iter().all(|entry| entry.score > 0.0));
    }

    /// End-to-end exercise of `build_adaptive_provider_chain` that
    /// covers the QoS plumbing surface, not just smoke survival:
    ///   (a) `AdaptiveRouter` is built when >1 provider survives;
    ///   (b) the seed catalog is actually consumed — we use entries
    ///       keyed by `ollama/llama3.2` so they line up with the
    ///       router lane the helper just built, then assert the
    ///       persisted catalog carries those seeded fields (cost_in,
    ///       context_window, model_type) instead of bare defaults;
    ///   (c) `provider_baseline.json` is loaded from `data_dir` when
    ///       present (non-cold-start path), and the latency/stability
    ///       values it carries show up in `octos_llm::context` /
    ///       `octos_llm::pricing` seeding through the exported
    ///       catalog;
    ///   (d) a deliberately-broken third fallback gets skipped via
    ///       `warn!` without taking the helper down;
    ///   (e) `model_catalog.json` on disk after the helper runs is
    ///       different from the cold seed — i.e. persistence wrote
    ///       new state, not just left the seed file untouched.
    #[test]
    fn build_adaptive_provider_chain_seeds_qos_plumbing_end_to_end() {
        use crate::config::{AdaptiveRoutingConfig, Config, FallbackModel};
        use octos_core::Message;
        use octos_llm::{ChatConfig, ChatResponse, LlmProvider, ToolSpec};
        use std::sync::Arc;

        struct StubProvider;
        #[async_trait::async_trait]
        impl LlmProvider for StubProvider {
            async fn chat(
                &self,
                _messages: &[Message],
                _tools: &[ToolSpec],
                _config: &ChatConfig,
            ) -> eyre::Result<ChatResponse> {
                Err(eyre::eyre!("stub not callable in tests"))
            }
            fn model_id(&self) -> &str {
                "stub-model"
            }
            fn provider_name(&self) -> &str {
                "stub"
            }
        }

        let temp = tempdir().unwrap();
        let data_dir = temp.path().to_path_buf();

        // We don't know the exact AdaptiveRouter lane labels up front
        // (the OpenAI-flavored providers tag their label with the
        // host suffix when a non-default base_url is set, e.g.
        // `ollama@localhost:11434/llama3.2`). Do a discovery pass
        // first to learn the real lane keys, then rebuild the seed
        // catalog + baseline so the helper's seed_catalog/seed_baseline
        // attaches them to the right slots when we re-run.

        let config = Config {
            provider: Some("stub".into()),
            fallback_models: vec![
                FallbackModel {
                    provider: "ollama".into(),
                    model: Some("llama3.2".into()),
                    base_url: None,
                    api_key_env: None,
                    model_hints: None,
                    api_type: None,
                    cost_per_m: Some(0.5),
                    strong: true,
                    context_window: None,
                },
                // Deliberately-broken third fallback — must be skipped
                // via `warn!` without taking the helper down.
                FallbackModel {
                    provider: "nope-not-a-real-provider".into(),
                    model: None,
                    base_url: None,
                    api_key_env: None,
                    model_hints: None,
                    api_type: None,
                    cost_per_m: None,
                    strong: true,
                    context_window: None,
                },
            ],
            // A1: AdaptiveRoutingConfig::default() now has `enabled = false`
            // and is a *no-op*. Tests that exercise the adaptive code path
            // must opt in explicitly.
            adaptive_routing: Some(AdaptiveRoutingConfig {
                enabled: true,
                ..AdaptiveRoutingConfig::default()
            }),
            ..Default::default()
        };

        // ─── Discovery pass: learn the real lane keys ───
        let base: Arc<dyn LlmProvider> = Arc::new(StubProvider);
        let discovery = build_adaptive_provider_chain(
            base.clone(),
            &config,
            &data_dir,
            false,
            ExporterMode::Disabled,
        );
        let discovery_runtime = discovery
            .runtime_qos_catalog
            .as_ref()
            .expect("discovery pass should produce a runtime catalog");
        let lane_keys: Vec<String> = discovery_runtime
            .models
            .iter()
            .map(|m| m.provider.clone())
            .collect();
        // (d) The broken third fallback was skipped via `warn!` — only
        // 2 lanes should survive.
        assert_eq!(
            lane_keys.len(),
            2,
            "broken fallback should be skipped via warn!, leaving 2 lanes; got {lane_keys:?}"
        );
        let stub_key = lane_keys
            .iter()
            .find(|k| k.starts_with("stub/"))
            .expect("primary stub lane must exist")
            .clone();
        let ollama_key = lane_keys
            .iter()
            .find(|k| k.starts_with("ollama") && k.ends_with("/llama3.2"))
            .expect("ollama fallback lane must exist")
            .clone();

        // ─── Real pass: seed catalog + baseline with the discovered
        // lane keys, then re-run the helper and assert the seed values
        // propagate into the persisted catalog. ───
        let matched_seed = QosCatalog {
            updated_at: "2026-04-11T00:00:00Z".to_string(),
            models: vec![
                ModelCatalogEntry {
                    provider: stub_key.clone(),
                    model_type: ModelType::Fast,
                    is_family_default: false,
                    stability: 0.95,
                    tool_avg_ms: 700,
                    p95_ms: 1100,
                    score: 0.0,
                    cost_in: 0.4,
                    cost_out: 1.6,
                    ds_output: 1000,
                    context_window: 64_000,
                    max_output: 4_096,
                },
                ModelCatalogEntry {
                    provider: ollama_key.clone(),
                    model_type: ModelType::Strong,
                    is_family_default: false,
                    stability: 0.88,
                    tool_avg_ms: 1800,
                    p95_ms: 3200,
                    score: 0.0,
                    cost_in: 0.0,
                    cost_out: 0.0,
                    ds_output: 600,
                    context_window: 128_000,
                    max_output: 8_192,
                },
            ],
        };
        std::fs::write(
            data_dir.join("model_catalog.json"),
            serde_json::to_string_pretty(&matched_seed).unwrap(),
        )
        .unwrap();
        // Use the exact field names BaselineEntry deserializes
        // (`avg_latency_ms` / `p95_latency_ms`) — and use a stability
        // value that DIFFERS from the seed catalog's, so we can tell
        // whether `seed_baseline` actually ran from the EMA-blended
        // result.
        let baseline = serde_json::json!([
            {
                "provider": stub_key,
                "avg_latency_ms": 700,
                "p95_latency_ms": 1100,
                "stability": 0.6
            },
            {
                "provider": ollama_key,
                "avg_latency_ms": 1800,
                "p95_latency_ms": 3200,
                "stability": 0.6
            }
        ]);
        std::fs::write(
            data_dir.join("provider_baseline.json"),
            serde_json::to_string_pretty(&baseline).unwrap(),
        )
        .unwrap();

        let bundle =
            build_adaptive_provider_chain(base, &config, &data_dir, false, ExporterMode::Disabled);

        // (a) AdaptiveRouter built.
        assert!(
            bundle.adaptive_router.is_some(),
            "AdaptiveRouter should be present when fallback build succeeds"
        );

        // (b) The RUNNING process's runtime catalog converges to the canonical
        //     SSOT for STATIC fields, exactly like the persisted file in (e) —
        //     not only the on-disk file. The seed is merged with the embedded
        //     canonical base BEFORE it seeds the router, so the host-tagged
        //     `ollama@…/llama3.2` lane carries the canonical context window
        //     (131072) and max output (131072), NOT the stale on-disk seed's
        //     128000 / 8192. Without that pre-seed merge the live process would
        //     route/cost/size with the stale values until the next restart.
        let runtime = bundle
            .runtime_qos_catalog
            .as_ref()
            .expect("seed catalog should produce a runtime catalog");
        let ollama_entry = runtime
            .models
            .iter()
            .find(|m| m.provider == ollama_key)
            .expect("ollama lane should be present in runtime catalog");
        assert_eq!(
            ollama_entry.context_window, 131_072,
            "runtime lane uses the canonical static context window, not the stale on-disk seed"
        );
        assert_eq!(
            ollama_entry.max_output, 131_072,
            "runtime lane uses the canonical static max output, not the stale on-disk seed"
        );
        assert_eq!(
            ollama_entry.model_type,
            ModelType::Strong,
            "model_type carries through (canonical and seed agree here)"
        );

        // The overlay-only `stub/stub-model` lane has no canonical entry, so the
        // running process keeps its seed static verbatim — proving the pre-seed
        // merge is field-level (canonical for known lanes, verbatim otherwise),
        // not a blanket replacement.
        let stub_entry = runtime
            .models
            .iter()
            .find(|m| m.provider == stub_key)
            .expect("stub lane should be present in runtime catalog");
        assert_eq!(
            stub_entry.context_window, 64_000,
            "overlay-only lane keeps its seed static context window"
        );
        assert_eq!(
            stub_entry.max_output, 4_096,
            "overlay-only lane keeps its seed static max output"
        );

        // (c) `seed_baseline` actually ran. We know because:
        //     - seed_catalog set baseline_stability = 0.88;
        //     - seed_baseline set success/failure counts that imply
        //       live_stab ≈ 0.6 (the value in the baseline fixture)
        //       and pushed total_requests to 10, giving the EMA
        //       blender weight = min(0.5, 10/20) = 0.5;
        //     - exported stability = 0.88 * 0.5 + ~0.6 * 0.5 ≈ 0.74.
        //     If seed_baseline had silently failed to load (e.g.
        //     wrong JSON field names took the warn path), total
        //     would be 0, weight 0, and the exported stability
        //     would round-trip 0.88 unchanged. Asserting strict
        //     inequality with both extremes catches that regression.
        assert!(
            ollama_entry.stability < 0.85 && ollama_entry.stability > 0.65,
            "blended stability must be strictly between baseline (0.6) and \
             seed-catalog (0.88), proving seed_baseline ran — got {}",
            ollama_entry.stability
        );

        // (e) persisted file reflects runtime DYNAMIC state while converging to
        //     the canonical SSOT for STATIC fields. The ollama lane's provider
        //     key carries the OpenAI-flavored host suffix (`ollama@…/llama3.2`);
        //     the merge strips that tag to reconcile it with the embedded
        //     canonical `ollama/llama3.2`, so the persisted lane gets the
        //     canonical static context window (131072, NOT the seed's 128000)
        //     with the blended runtime stability layered on.
        let persisted_json = std::fs::read_to_string(data_dir.join("model_catalog.json"))
            .expect("persisted catalog readable");
        let persisted: QosCatalog = serde_json::from_str(&persisted_json).unwrap();
        let persisted_ollama = persisted
            .models
            .iter()
            .find(|m| m.provider == ollama_key)
            .expect("ollama lane should be in persisted catalog");
        // Dynamic runtime state persisted (blended stability, not the raw 0.88).
        assert!(
            persisted_ollama.stability < 0.85 && persisted_ollama.stability > 0.65,
            "persisted stability is the blended runtime value: {}",
            persisted_ollama.stability
        );
        // Static converged to the canonical catalog despite the host-tagged key.
        assert_eq!(persisted_ollama.context_window, 131_072);
        assert_eq!(persisted_ollama.max_output, 131_072);
    }

    /// A1 regression: `adaptive_routing.enabled = false` MUST NOT
    /// instantiate an `AdaptiveRouter`. Before this fix, the helper
    /// silently defaulted to `mode=Lane, qos=true` whenever
    /// `providers.len() > 1`, ignoring `enabled` entirely — which the
    /// investigation report called out as a config-correctness bug.
    #[test]
    fn build_adaptive_provider_chain_respects_disabled_flag() {
        use crate::config::{AdaptiveRoutingConfig, Config, FallbackModel};
        use octos_core::Message;
        use octos_llm::{ChatConfig, ChatResponse, LlmProvider, ToolSpec};
        use std::sync::Arc;

        struct StubProvider;
        #[async_trait::async_trait]
        impl LlmProvider for StubProvider {
            async fn chat(
                &self,
                _messages: &[Message],
                _tools: &[ToolSpec],
                _config: &ChatConfig,
            ) -> eyre::Result<ChatResponse> {
                Err(eyre::eyre!("stub not callable in tests"))
            }
            fn model_id(&self) -> &str {
                "stub-model"
            }
            fn provider_name(&self) -> &str {
                "stub"
            }
        }

        let temp = tempdir().unwrap();
        let data_dir = temp.path().to_path_buf();

        // Two providers AND adaptive_routing.enabled = false →
        // no AdaptiveRouter must be built. Previously this would have
        // wrapped silently because providers.len() > 1.
        let config = Config {
            provider: Some("stub".into()),
            fallback_models: vec![FallbackModel {
                provider: "ollama".into(),
                model: Some("llama3.2".into()),
                base_url: None,
                api_key_env: None,
                model_hints: None,
                api_type: None,
                cost_per_m: Some(0.5),
                strong: true,
                context_window: None,
            }],
            adaptive_routing: Some(AdaptiveRoutingConfig {
                enabled: false,
                ..AdaptiveRoutingConfig::default()
            }),
            ..Default::default()
        };

        let base: Arc<dyn LlmProvider> = Arc::new(StubProvider);
        let bundle =
            build_adaptive_provider_chain(base, &config, &data_dir, false, ExporterMode::Disabled);

        assert!(
            bundle.adaptive_router.is_none(),
            "enabled = false MUST NOT instantiate an AdaptiveRouter"
        );
    }

    /// A1 regression: when `adaptive_routing` is entirely absent from
    /// the config (`None`), the helper must NOT silently default-ON.
    /// Previously the unwrap_or path quietly picked `Lane + qos=true`.
    #[test]
    fn build_adaptive_provider_chain_defaults_off_when_config_absent() {
        use crate::config::{Config, FallbackModel};
        use octos_core::Message;
        use octos_llm::{ChatConfig, ChatResponse, LlmProvider, ToolSpec};
        use std::sync::Arc;

        struct StubProvider;
        #[async_trait::async_trait]
        impl LlmProvider for StubProvider {
            async fn chat(
                &self,
                _messages: &[Message],
                _tools: &[ToolSpec],
                _config: &ChatConfig,
            ) -> eyre::Result<ChatResponse> {
                Err(eyre::eyre!("stub not callable in tests"))
            }
            fn model_id(&self) -> &str {
                "stub-model"
            }
            fn provider_name(&self) -> &str {
                "stub"
            }
        }

        let temp = tempdir().unwrap();
        let data_dir = temp.path().to_path_buf();

        let config = Config {
            provider: Some("stub".into()),
            fallback_models: vec![FallbackModel {
                provider: "ollama".into(),
                model: Some("llama3.2".into()),
                base_url: None,
                api_key_env: None,
                model_hints: None,
                api_type: None,
                cost_per_m: Some(0.5),
                strong: true,
                context_window: None,
            }],
            adaptive_routing: None,
            ..Default::default()
        };

        let base: Arc<dyn LlmProvider> = Arc::new(StubProvider);
        let bundle =
            build_adaptive_provider_chain(base, &config, &data_dir, false, ExporterMode::Disabled);

        assert!(
            bundle.adaptive_router.is_none(),
            "missing adaptive_routing block MUST default to OFF (no router)"
        );
    }

    /// #2142: an operator `context_window` override must resolve through the
    /// WHOLE assembled stack (RetryProvider here), beating what the underlying
    /// provider reports — the acceptance criterion "a profile pinning
    /// context_window: 16384 on a 262K server reports 16384 through the full
    /// runtime stack".
    #[test]
    fn context_window_override_wins_through_the_assembled_stack() {
        use crate::config::Config;
        use octos_core::Message;
        use octos_llm::{ChatConfig, ChatResponse, LlmProvider, ToolSpec};
        use std::sync::Arc;

        // A backend that advertises a large window (stands in for the probed
        // 262K llama-server).
        struct WideProvider;
        #[async_trait::async_trait]
        impl LlmProvider for WideProvider {
            async fn chat(
                &self,
                _messages: &[Message],
                _tools: &[ToolSpec],
                _config: &ChatConfig,
            ) -> eyre::Result<ChatResponse> {
                Err(eyre::eyre!("stub not callable in tests"))
            }
            fn model_id(&self) -> &str {
                "wide-model"
            }
            fn provider_name(&self) -> &str {
                "wide"
            }
            fn context_window(&self) -> u32 {
                262_144
            }
        }

        let temp = tempdir().unwrap();
        let data_dir = temp.path().to_path_buf();

        // Control: no override → the backend's own window survives the
        // RetryProvider wrap (delegation, per #2135).
        let control = build_adaptive_provider_chain(
            Arc::new(WideProvider),
            &Config::default(),
            &data_dir,
            false,
            ExporterMode::Disabled,
        );
        assert_eq!(
            control.llm.context_window(),
            262_144,
            "without an override the probed/backend window must pass through the stack"
        );

        // Override: 16384 must win through RetryProvider all the way out.
        let config = Config {
            context_window: Some(16_384),
            ..Default::default()
        };
        let overridden = build_adaptive_provider_chain(
            Arc::new(WideProvider),
            &config,
            &data_dir,
            false,
            ExporterMode::Disabled,
        );
        assert_eq!(
            overridden.llm.context_window(),
            16_384,
            "config.context_window must override the 262K backend through the full stack"
        );

        // And in the no_retry path (bare provider) the override still holds.
        let bare = build_adaptive_provider_chain(
            Arc::new(WideProvider),
            &config,
            &data_dir,
            true,
            ExporterMode::Disabled,
        );
        assert_eq!(
            bare.llm.context_window(),
            16_384,
            "override must hold even on the no_retry (unwrapped) path"
        );
    }
}
