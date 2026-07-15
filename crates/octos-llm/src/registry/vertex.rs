use std::sync::Arc;

use eyre::{Result, WrapErr};

use crate::gemini::GeminiProvider;
use crate::provider::LlmProvider;
use crate::vertex_auth::ServiceAccount;

use super::{CreateParams, ProviderEntry};

/// Vertex AI (Gemini) provider — authenticates with a Google service-account
/// JSON instead of an API key. The GCP project is read from the JSON's
/// `project_id`; the region is fixed to `global`.
///
/// Selected explicitly via `provider: "vertex"` (no auto-detection from model
/// name — bare `gemini-*` models still resolve to the AI Studio `gemini`
/// provider). The credential is the service-account JSON *content*, resolved
/// through the normal key channel under `VERTEX_SA_JSON` (a keychain marker →
/// macOS Keychain, a plain config value, or the process env). The private key
/// never has to live as a file path.
pub const ENTRY: ProviderEntry = ProviderEntry {
    name: "vertex",
    aliases: &["vertex-ai", "vertexai"],
    default_model: Some("gemini-2.5-flash"),
    api_key_env: Some("VERTEX_SA_JSON"),
    key_env_aliases: &[],
    default_base_url: None,
    requires_api_key: true,
    requires_base_url: false,
    requires_model: false,
    detect_patterns: &[],
    create,
};

fn create(p: CreateParams) -> Result<Arc<dyn LlmProvider>> {
    let http_timeout = p.http_timeout();
    let json = p.api_key.ok_or_else(|| {
        eyre::eyre!(
            "vertex provider requires a service-account JSON credential \
             (VERTEX_SA_JSON — store it via the dashboard or `octos auth`)"
        )
    })?;
    let sa = ServiceAccount::from_json(&json)
        .wrap_err("failed to parse Vertex service-account JSON credential")?;
    let model = p.model.unwrap_or_else(|| "gemini-2.5-flash".into());
    // Thread the timeout into both the chat client (`with_http_timeout`) and the
    // OAuth token-exchange client (the `_with_timeout` constructor).
    let mut provider =
        GeminiProvider::vertex_from_service_account_with_timeout(sa, &model, http_timeout);
    if let Some((t, c)) = http_timeout {
        provider = provider.with_http_timeout(t, c);
    }
    Ok(Arc::new(provider))
}
