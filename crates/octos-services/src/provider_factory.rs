use std::sync::Arc;

use eyre::Result;
use octos_llm::LlmProvider;

use crate::config::Config;

/// Inner factory that accepts an explicit `api_type` override.
///
/// Does NOT print to stdout — callers that want a log line should print
/// after calling this function.
///
/// Exposed as `pub` (was `pub(crate)`) so the `octos-ffi` C-ABI crate can
/// reuse the exact provider-construction path (auth store → env_vars → env
/// key resolution, timeout overrides, anthropic/responses api_type bypasses)
/// instead of re-implementing a drift-prone parallel factory.
pub fn create_provider_with_api_type(
    name: &str,
    config: &Config,
    model: Option<String>,
    base_url: Option<String>,
    api_type: Option<&str>,
) -> Result<Arc<dyn LlmProvider>> {
    if name == "custom" {
        return create_custom_provider(config, model, base_url, api_type);
    }

    let entry = octos_llm::registry::lookup(name).ok_or_else(|| {
        eyre::eyre!(
            "unknown provider: {name}. Valid: {}",
            octos_llm::registry::all_names().join(", ")
        )
    })?;

    // Resolve API key via config (auth store → env var).
    let api_key = if entry.requires_api_key {
        Some(config.get_api_key(entry.name)?)
    } else {
        config.get_api_key(entry.name).ok()
    };

    if entry.requires_model && model.is_none() {
        eyre::bail!("{} provider requires --model to be specified", name);
    }
    if entry.requires_base_url && base_url.is_none() {
        eyre::bail!("{} provider requires --base-url to be specified", name);
    }

    // Extract timeout overrides from gateway config (if any).
    let llm_timeout_secs = config.gateway.as_ref().and_then(|g| g.llm_timeout_secs);
    let llm_connect_timeout_secs = config
        .gateway
        .as_ref()
        .and_then(|g| g.llm_connect_timeout_secs);

    // If api_type is "anthropic", bypass registry and use AnthropicProvider directly.
    // This allows any provider to use the Anthropic Messages API protocol.
    if api_type == Some("anthropic") {
        let key = api_key.ok_or_else(|| eyre::eyre!("API key required for anthropic api_type"))?;
        let m = model
            .or_else(|| entry.default_model().map(str::to_string))
            .ok_or_else(|| {
                eyre::eyre!(
                    "{}: no model given and the catalog declares no default for this family",
                    entry.name
                )
            })?;
        // Without a base_url, use the root the family serves the Anthropic
        // protocol at — for z.ai that is not the lane's (OpenAI) default.
        let url = base_url.unwrap_or_else(|| {
            octos_llm::registry::anthropic_api_type_default_root(entry)
                .unwrap_or("https://api.anthropic.com")
                .into()
        });
        let mut provider =
            octos_llm::anthropic::AnthropicProvider::new(&key, &m).with_base_url(&url);
        if let Some(t) = llm_timeout_secs {
            let c = llm_connect_timeout_secs.unwrap_or(octos_llm::DEFAULT_LLM_CONNECT_TIMEOUT_SECS);
            provider = provider.with_http_timeout(t, c);
        }
        return Ok(Arc::new(provider));
    }

    // If api_type is "responses", use OpenAI Responses API directly.
    // This forces the Responses API even for models not auto-detected.
    if api_type == Some("responses") {
        let key = api_key.ok_or_else(|| eyre::eyre!("API key required for responses api_type"))?;
        let m = model
            .or_else(|| entry.default_model().map(str::to_string))
            .ok_or_else(|| {
                eyre::eyre!(
                    "{}: no model given and the catalog declares no default for this family",
                    entry.name
                )
            })?;
        let mut provider = octos_llm::openai_responses::OpenAIResponsesProvider::new(&key, &m)
            .with_response_continuation(true);
        if let Some(url) = base_url.as_ref() {
            provider = provider.with_base_url(url.as_str());
        }
        if let Some(t) = llm_timeout_secs {
            let c = llm_connect_timeout_secs.unwrap_or(octos_llm::DEFAULT_LLM_CONNECT_TIMEOUT_SECS);
            provider = provider.with_http_timeout(t, c);
        }
        let provider: Arc<dyn LlmProvider> = Arc::new(provider);
        if let Some(url) = base_url
            .as_deref()
            .filter(|url| url.trim_end_matches('/') != "https://api.openai.com/v1")
        {
            let timeout = match (llm_timeout_secs, llm_connect_timeout_secs) {
                (None, None) => None,
                (t, c) => Some((
                    t.unwrap_or(octos_llm::DEFAULT_LLM_TIMEOUT_SECS),
                    c.unwrap_or(octos_llm::DEFAULT_LLM_CONNECT_TIMEOUT_SECS),
                )),
            };
            return Ok(octos_llm::LocalContextProbe::new(
                provider,
                url,
                Some(key),
                timeout,
            ));
        }
        return Ok(provider);
    }

    let params = octos_llm::registry::CreateParams {
        api_key,
        model,
        base_url,
        model_hints: config.model_hints.clone(),
        llm_timeout_secs,
        llm_connect_timeout_secs,
    };

    let provider = (entry.create)(params)?;
    Ok(provider)
}

fn create_custom_provider(
    config: &Config,
    model: Option<String>,
    base_url: Option<String>,
    api_type: Option<&str>,
) -> Result<Arc<dyn LlmProvider>> {
    let key = config.get_api_key("custom")?;
    let model = model.ok_or_else(|| eyre::eyre!("custom provider requires model"))?;
    let base_url = base_url.ok_or_else(|| eyre::eyre!("custom provider requires base_url"))?;

    // Extract timeout overrides from gateway config (if any).
    let llm_timeout_secs = config.gateway.as_ref().and_then(|g| g.llm_timeout_secs);
    let llm_connect_timeout_secs = config
        .gateway
        .as_ref()
        .and_then(|g| g.llm_connect_timeout_secs);

    match api_type.unwrap_or("openai") {
        "openai" => {
            let mut provider = octos_llm::openai::OpenAIProvider::new(&key, model)
                .with_base_url(&base_url)
                .with_provider_label("custom");
            let http_timeout = llm_timeout_secs.map(|t| {
                (
                    t,
                    llm_connect_timeout_secs.unwrap_or(octos_llm::DEFAULT_LLM_CONNECT_TIMEOUT_SECS),
                )
            });
            if let Some((t, c)) = http_timeout {
                provider = provider.with_http_timeout(t, c);
            }
            // This factory bypasses the registry. Preserve the same runtime
            // context discovery as the openai/local registry paths.
            Ok(octos_llm::LocalContextProbe::new(
                Arc::new(provider),
                &base_url,
                Some(key),
                http_timeout,
            ))
        }
        "anthropic" => {
            let mut provider = octos_llm::anthropic::AnthropicProvider::new(key, model)
                .with_base_url(&base_url)
                // #2194: label stays "custom" (its logical identity for
                // adaptive-lane / QoS matching); the Anthropic cache rate is
                // carried by ProviderMetadata::cache_lane, set from the provider
                // TYPE in AnthropicProvider::provider_metadata().
                .with_provider_label("custom");
            if let Some(t) = llm_timeout_secs {
                let c =
                    llm_connect_timeout_secs.unwrap_or(octos_llm::DEFAULT_LLM_CONNECT_TIMEOUT_SECS);
                provider = provider.with_http_timeout(t, c);
            }
            Ok(Arc::new(provider))
        }
        other => eyre::bail!("unsupported custom api_type '{other}'; use openai or anthropic"),
    }
}

#[cfg(test)]
mod custom_provider_tests {
    use super::*;

    fn custom_config() -> Config {
        let mut config = Config {
            api_key_env: Some("CUSTOM_API_KEY".to_string()),
            ..Default::default()
        };
        config
            .env_vars
            .insert("CUSTOM_API_KEY".to_string(), "test-key".to_string());
        config
    }

    #[tokio::test]
    async fn custom_openai_provider_discovers_window_before_compaction() {
        use wiremock::matchers::{header, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "qwen3.8-27b", "max_model_len": 262_144}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let provider = create_provider_with_api_type(
            "custom",
            &custom_config(),
            Some("qwen3.8-27b".into()),
            Some(format!("{}/v1", server.uri())),
            Some("openai"),
        )
        .unwrap();

        provider.ensure_ready().await;
        assert_eq!(provider.context_window(), 262_144);
        assert_eq!(provider.provider_name(), "custom");
        assert_eq!(provider.model_id(), "qwen3.8-27b");
        assert_eq!(
            provider.provider_metadata().cache_lane,
            octos_llm::CacheLane::Residual
        );
    }

    #[test]
    fn creates_custom_openai_compatible_provider() {
        let provider = create_provider_with_api_type(
            "custom",
            &custom_config(),
            Some("llama-3.1-70b-instruct".to_string()),
            Some("http://127.0.0.1:11434/v1".to_string()),
            Some("openai"),
        )
        .unwrap();

        assert_eq!(provider.provider_name(), "custom");
        assert_eq!(provider.model_id(), "llama-3.1-70b-instruct");
        // #2194 R4: a custom OpenAI endpoint keeps the "custom" identity AND the
        // residual cache lane (full-rate reads) — NOT the Anthropic 0.1x bucket.
        let meta = provider.provider_metadata();
        assert_eq!(meta.cache_lane, octos_llm::CacheLane::Residual);
        assert_eq!(
            octos_llm::pricing::cache_rates_for_lane(meta.cache_lane).read_multiplier,
            1.0,
            "custom + api_type=openai must price cache reads at the residual rate",
        );
    }

    #[test]
    fn creates_custom_anthropic_compatible_provider() {
        let provider = create_provider_with_api_type(
            "custom",
            &custom_config(),
            Some("claude-compatible".to_string()),
            Some("https://proxy.example.com/anthropic".to_string()),
            Some("anthropic"),
        )
        .unwrap();

        // #2194 R4: the label STAYS "custom" (its logical identity for
        // adaptive-lane / QoS matching — relabeling it silently disabled a
        // configured lane restriction). The Anthropic cache rate is instead
        // carried by the metadata cache_lane, sourced from the provider TYPE,
        // so pricing is correct WITHOUT overloading the identity label.
        assert_eq!(provider.provider_name(), "custom");
        assert_eq!(provider.model_id(), "claude-compatible");
        let meta = provider.provider_metadata();
        assert_eq!(
            meta.cache_lane,
            octos_llm::CacheLane::Anthropic,
            "custom + api_type=anthropic must carry the Anthropic cache lane",
        );
        assert_eq!(
            octos_llm::pricing::cache_rates_for_lane(meta.cache_lane).read_multiplier,
            0.1,
            "and therefore price cache reads at 0.1x, not the 1.0x residual",
        );
    }

    #[test]
    fn rejects_custom_provider_without_base_url() {
        let result = create_provider_with_api_type(
            "custom",
            &custom_config(),
            Some("llama".to_string()),
            None,
            Some("openai"),
        );

        match result {
            Ok(provider) => panic!(
                "expected missing base_url error, got provider {}",
                provider.provider_name()
            ),
            Err(err) => assert!(err.to_string().contains("requires base_url")),
        }
    }
}
