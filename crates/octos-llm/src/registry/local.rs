use std::sync::Arc;

use eyre::Result;

use crate::openai::OpenAIProvider;
use crate::provider::LlmProvider;

use super::{CreateParams, ProviderEntry};

/// Fallback model id when neither config nor catalog names one. Single-model
/// local servers (llama.cpp, LM Studio, vLLM) ignore the request's `model`
/// field, so any placeholder works against them.
const FALLBACK_MODEL: &str = "default";

/// Fallback base URL: llama.cpp's `llama-server` default. Ollama (11434),
/// vLLM (8000), and LM Studio (1234) users set `base_url` — or use the
/// `ollama`/`vllm` families, which remain registered with their own defaults.
const FALLBACK_BASE_URL: &str = "http://127.0.0.1:8080/v1";

/// The unified local family: ONE onboarding choice for every OpenAI-compatible
/// local server (llama.cpp, Ollama, vLLM, LM Studio, …). The engines differ
/// only operationally (port, whether the `model` field selects anything,
/// whether a key was configured), so the entry requires nothing and defaults
/// everything.
pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "local",
    aliases: &[
        "llamacpp",
        "llama.cpp",
        "llama-server",
        "llama_server",
        "lmstudio",
        "lm-studio",
        "openai-compatible",
    ],
    // Keyless by default; a key set via config `api_key_env` still reaches
    // `create` through the normal resolution chain (llama-server --api-key).
    api_key_env: None,
    key_env_aliases: &[],
    default_base_url: Some(FALLBACK_BASE_URL),
    requires_api_key: false,
    requires_base_url: false,
    requires_model: false,
    // User-loaded models — nothing to pattern-match.
    detect_patterns: &[],
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    // Unlike other families, a missing model is not an error: fall back to a
    // placeholder rather than failing, because the common local server hosts
    // exactly one model and ignores the field. (Ollama-style servers where the
    // name selects a model need it in config — doctor cross-checks that.)
    let model = p
        .model
        .or_else(|| ENTRY.default_model().map(str::to_string))
        .unwrap_or_else(|| FALLBACK_MODEL.into());
    let url = p.base_url.unwrap_or_else(|| FALLBACK_BASE_URL.into());
    let key = p.api_key.unwrap_or_else(|| "no-key".into());
    let mut provider = OpenAIProvider::new(&key, &model)
        .with_provider_label("local")
        .with_base_url(&url);
    if let Some(hints) = p.model_hints {
        provider = provider.with_hints(hints);
    }
    if let Some((t, c)) = http_timeout {
        provider = provider.with_http_timeout(t, c);
    }
    Ok(Arc::new(provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_params() -> CreateParams {
        CreateParams {
            api_key: None,
            model: None,
            base_url: None,
            model_hints: None,
            llm_timeout_secs: None,
            llm_connect_timeout_secs: None,
        }
    }

    /// The whole point of the family: zero config constructs a working
    /// provider (key, model, and base URL all defaulted).
    #[test]
    fn should_construct_with_no_key_no_model_no_base_url() {
        assert!(create(empty_params()).is_ok());
    }

    /// An Ollama-style setup (named model, custom port, no key) also works.
    #[test]
    fn should_accept_explicit_model_and_base_url() {
        let params = CreateParams {
            model: Some("llama3.2".into()),
            base_url: Some("http://127.0.0.1:11434/v1".into()),
            ..empty_params()
        };
        assert!(create(params).is_ok());
    }
}
