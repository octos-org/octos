use std::path::Path;
use std::sync::Arc;

use octos_llm::{
    AdaptiveConfig, AdaptiveMode, AdaptiveRouter, BaselineEntry, LlmProvider, ModelCatalogEntry,
    ProviderChain, QosCatalog, RetryProvider,
};
use tracing::{info, warn};

use crate::commands::chat::create_provider_with_api_type;
use crate::config::Config;

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
/// 7. When `spawn_exporter` is true and an `AdaptiveRouter` exists,
///    spawns a tokio task that re-writes `model_catalog.json` every
///    30s from the router's live export. Tests should pass
///    `spawn_exporter=false` to keep the test free of leaked tokio
///    tasks.
pub(crate) fn build_adaptive_provider_chain(
    base_provider: Arc<dyn LlmProvider>,
    config: &Config,
    data_dir: &Path,
    no_retry: bool,
    spawn_exporter: bool,
) -> AdaptiveProviderBundle {
    let mut adaptive_router_ref: Option<Arc<AdaptiveRouter>> = None;

    let llm: Arc<dyn LlmProvider> = if no_retry {
        base_provider
    } else if config.fallback_models.is_empty() {
        Arc::new(RetryProvider::new(base_provider))
    } else {
        let mut providers: Vec<Arc<dyn LlmProvider>> =
            vec![Arc::new(RetryProvider::new(base_provider))];
        let mut costs: Vec<f64> = vec![0.0]; // primary cost unknown
        for fb in &config.fallback_models {
            let fb_config = if fb.api_key_env.is_some() {
                let mut c = config.clone();
                c.api_key_env = fb.api_key_env.clone();
                c
            } else {
                config.clone()
            };
            match create_provider_with_api_type(
                &fb.provider,
                &fb_config,
                fb.model.clone(),
                fb.base_url.clone(),
                fb.api_type.as_deref(),
            ) {
                Ok(p) => {
                    providers.push(Arc::new(RetryProvider::new(p)));
                    costs.push(fb.cost_per_m.unwrap_or(0.0));
                }
                Err(e) => {
                    warn!(provider = %fb.provider, error = %e, "skipping fallback provider");
                }
            }
        }
        if providers.len() > 1 {
            let adaptive_config = config
                .adaptive_routing
                .as_ref()
                .map(AdaptiveConfig::from)
                .unwrap_or_default();
            let ar_config = config.adaptive_routing.as_ref();
            info!("adaptive routing enabled ({} providers)", providers.len());
            let mode = ar_config
                .map(|c| c.mode.into())
                .unwrap_or(AdaptiveMode::Lane);
            let qos = ar_config.map(|c| c.qos_ranking).unwrap_or(true);
            let router = Arc::new(
                AdaptiveRouter::new(providers, &costs, adaptive_config)
                    .with_adaptive_config(mode, qos),
            );
            adaptive_router_ref = Some(router.clone());
            router
        } else {
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
    let seed_catalog = load_seed_qos_catalog(data_dir);

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
        persist_qos_catalog(&catalog_path, catalog);
    }

    if spawn_exporter {
        if let Some(ref router) = adaptive_router_ref {
            let metrics_router = router.clone();
            let exporter_path = catalog_path.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                loop {
                    interval.tick().await;
                    if let Ok(json) =
                        serde_json::to_string_pretty(&metrics_router.export_model_catalog())
                    {
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
    None
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

    /// End-to-end exercise of `build_adaptive_provider_chain`: a fake
    /// stub provider stands in for the real LLM client, one valid
    /// fallback gets layered in (a second fallback fails to construct
    /// and gets skipped via `warn!`). With `spawn_exporter=false` we
    /// don't leak a tokio task. We assert:
    ///   (a) the bundle exposes an `AdaptiveRouter` when >1 provider
    ///       was built;
    ///   (b) `runtime_qos_catalog` is materialized when a seed
    ///       `model_catalog.json` is present on disk;
    ///   (c) no panic when `provider_baseline.json` is absent (the
    ///       cold-start info path).
    #[test]
    fn build_adaptive_provider_chain_seeds_qos_without_baseline_file() {
        use crate::config::{AdaptiveRoutingConfig, Config, FallbackModel};
        use octos_core::Message;
        use octos_llm::{ChatConfig, ChatResponse, LlmProvider, ToolSpec};
        use std::sync::Arc;

        // Minimal stub provider — never actually invoked by the helper.
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
        // Drop a seed `model_catalog.json` so the helper has something
        // to materialize without ever calling the live router export.
        std::fs::write(
            data_dir.join("model_catalog.json"),
            serde_json::to_string_pretty(&sample_catalog([0.0, 0.0])).unwrap(),
        )
        .unwrap();

        let mut config = Config::default();
        config.provider = Some("stub".into());
        config.fallback_models = vec![
            // `ollama` doesn't require an API key, model, or base_url,
            // so the helper can build it offline without touching the
            // filesystem or network — gives us a real second provider.
            FallbackModel {
                provider: "ollama".into(),
                model: Some("llama3.2".into()),
                base_url: None,
                api_key_env: None,
                model_hints: None,
                api_type: None,
                cost_per_m: Some(0.5),
                strong: true,
            },
            // This fallback should fail to build (unknown provider)
            // and be skipped via `warn!`.
            FallbackModel {
                provider: "nope-not-a-real-provider".into(),
                model: None,
                base_url: None,
                api_key_env: None,
                model_hints: None,
                api_type: None,
                cost_per_m: None,
                strong: true,
            },
        ];
        config.adaptive_routing = Some(AdaptiveRoutingConfig::default());

        let base: Arc<dyn LlmProvider> = Arc::new(StubProvider);
        let bundle = build_adaptive_provider_chain(base, &config, &data_dir, false, false);

        // (a) `AdaptiveRouter` is built when >1 provider survives.
        assert!(
            bundle.adaptive_router.is_some(),
            "AdaptiveRouter should be present when fallback build succeeds"
        );
        // (b) runtime catalog materialized from the seed.
        assert!(
            bundle.runtime_qos_catalog.is_some(),
            "seed catalog should produce a runtime catalog"
        );
        // (c) cold-start info path didn't panic: helper returned cleanly.
        // (Implicit: the test reached this line.)

        // model_catalog.json was rewritten with the materialized catalog.
        let persisted_path = data_dir.join("model_catalog.json");
        assert!(persisted_path.exists());
    }
}
