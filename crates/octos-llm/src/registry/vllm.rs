use std::sync::Arc;

use eyre::Result;

use crate::local_context_probe::LocalContextProbe;
use crate::openai::OpenAIProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "vllm",
    aliases: &[],
    api_key_env: Some("VLLM_API_KEY"),
    key_env_aliases: &[],
    default_base_url: None,
    requires_api_key: false,
    requires_base_url: true,
    requires_model: true,
    // vLLM hosts user-deployed models — no detect pattern.
    detect_patterns: &[],
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    // The probe authenticates like the provider (`vllm --api-key`
    // deployments 401 anonymous GETs); capture before the placeholder.
    let probe_key = p.api_key.clone();
    let key = p.api_key.unwrap_or_else(|| "token".into());
    let model = p
        .model
        .ok_or_else(|| eyre::eyre!("vllm provider requires --model to be specified"))?;
    let url = p
        .base_url
        .ok_or_else(|| eyre::eyre!("vllm provider requires --base-url to be specified"))?;
    let mut provider = OpenAIProvider::new(&key, &model)
        .with_provider_label("vllm")
        .with_base_url(&url);
    if let Some(hints) = p.model_hints {
        provider = provider.with_hints(hints);
    }
    if let Some((t, c)) = http_timeout {
        provider = provider.with_http_timeout(t, c);
    }
    // Same class of local server as the `local` family: the catalog's
    // context_window is a guess, the running server knows (vLLM reports
    // max_model_len on /v1/models) — probe it (see `local_context_probe`).
    Ok(LocalContextProbe::new(
        Arc::new(provider),
        &url,
        probe_key,
        http_timeout,
    ))
}
