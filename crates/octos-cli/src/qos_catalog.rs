use std::path::Path;
use std::sync::Arc;

use eyre::WrapErr;
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
) -> eyre::Result<AdaptiveProviderBundle> {
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
                Err(error) => {
                    return Err(error).wrap_err_with(|| {
                        format!(
                            "configured fallback construction failed (operation=build_fallback_lane provider={} model={})",
                            fb.provider,
                            fb.model.as_deref().unwrap_or("<default>")
                        )
                    });
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

    Ok(AdaptiveProviderBundle {
        llm,
        adaptive_router: adaptive_router_ref,
        runtime_qos_catalog,
    })
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
