//! Automatic memory refreshing: background extraction over idle sessions.
//!
//! Design Layer 2 (PR-3 of the memory-refresh series). The pipeline runs
//! only in the long-running process that owns the profile's refresh lock
//! (serve or gateway; `octos chat` never runs background passes), gated by
//! `memory.refresh.enabled`.

pub(crate) mod extract;
pub(crate) mod input;
pub(crate) mod redact;
pub mod service;

pub use service::{MemoryRefreshService, PassReport, RefreshKnobs, refresh_status, run_once};

/// Resolve the provider for background memory passes.
///
/// `extract_model` accepts `provider/model` or a bare model id (provider
/// detected); unset → the profile's own provider. Resolution failures
/// warn and fall back to the profile provider — a bad knob must not
/// disable the sweep.
pub fn resolve_refresh_provider(
    config: &crate::config::Config,
    profile_provider: std::sync::Arc<dyn octos_llm::LlmProvider>,
    extract_model: Option<&str>,
) -> std::sync::Arc<dyn octos_llm::LlmProvider> {
    let Some(key) = extract_model.map(str::trim).filter(|k| !k.is_empty()) else {
        return profile_provider;
    };
    let (provider_name, model) = match key.split_once('/') {
        Some((p, m)) => (p.to_string(), Some(m.to_string())),
        None => match crate::config::detect_provider(key) {
            Some(p) => (p.to_string(), Some(key.to_string())),
            None => {
                tracing::warn!(
                    key,
                    "memory.refresh.extract_model has no recognizable provider; using the profile provider"
                );
                return profile_provider;
            }
        },
    };
    match crate::commands::chat::create_provider_with_api_type(
        &provider_name,
        config,
        model,
        None,
        config.api_type.as_deref(),
    ) {
        Ok(provider) => std::sync::Arc::new(octos_llm::RetryProvider::new(provider)),
        Err(e) => {
            tracing::warn!(
                key,
                "failed to build memory.refresh.extract_model provider ({e:#}); using the profile provider"
            );
            profile_provider
        }
    }
}
