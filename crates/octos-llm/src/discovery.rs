//! Protocol- and provider-aware model discovery.
//!
//! `profile/llm/fetch_models` (and the admin REST `/api/my/provider-models`
//! surface) used to switch on the literal family id `anthropic` and synthesize
//! a Bearer-authenticated `<base>/v1/models` probe for everything else. That
//! mis-probed Anthropic-protocol families whose ids differ (zai, zai-coding),
//! duplicated version segments on versioned OpenAI-compatible roots
//! (`.../v4` → `.../v4/v1/models`), and collapsed every failure mode into one
//! empty list.
//!
//! Instead, each registry entry declares its discovery capability
//! ([`ModelDiscovery`]) and the HTTP probe resolves from the selected ROUTE —
//! the same override rules the inference path applies in
//! `create_provider_with_api_type`: an `api_type` of `anthropic` forces the
//! Anthropic Messages strategy, everything else follows the family's declared
//! protocol (unknown families fall back to OpenAI-compatible, exactly like the
//! runtime's custom-provider fallback). Families whose protocol is selected by
//! MODEL NAME (r9s: `claude-*` → Anthropic) additionally carry a per-model
//! resolver so the probe follows the selected model's protocol AND rewritten
//! API root. URL joining is relative to the configured API root — no
//! heuristic version stripping/appending.
//!
//! Discovery is ADVISORY: `Unsupported` or a discovery-only 404 must never
//! mark a configured model unavailable and must never block manual model-id
//! entry, Test, or Save. The typed [`DiscoveryOutcome`] keeps "enter the model
//! id manually" distinguishable from "the credential/endpoint is invalid".

use std::time::Duration;

/// The wire protocol a discovery strategy speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryProtocol {
    /// Anthropic Messages listing: `GET {root}/v1/models` with `x-api-key`
    /// and `anthropic-version` headers; response `{data: [{id: ...}]}`.
    AnthropicMessages,
    /// OpenAI-compatible listing: `GET {root}/models` with `Authorization:
    /// Bearer` (header suppressed for keyless families); response
    /// `{data: [{id: ...}]}`.
    OpenAICompatible,
    /// Google Gemini listing: `GET {root}/models` with `x-goog-api-key`;
    /// response `{models: [{name: "models/..."}]}`.
    GeminiList,
}

/// A provider family's declared model-discovery capability. Stored on every
/// registry [`crate::registry::ProviderEntry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelDiscovery {
    /// Discovery is supported with the protocol-specific request strategy.
    Supported(DiscoveryProtocol),
    /// The family intentionally has no model-list endpoint — manual model id
    /// only. The payload is a stable, key-free reason for UIs to surface.
    Unsupported(&'static str),
}

/// Shorthand for registry entries: Anthropic Messages discovery.
pub const ANTHROPIC_MODELS: ModelDiscovery =
    ModelDiscovery::Supported(DiscoveryProtocol::AnthropicMessages);
/// Shorthand for registry entries: OpenAI-compatible discovery.
pub const OPENAI_MODELS: ModelDiscovery =
    ModelDiscovery::Supported(DiscoveryProtocol::OpenAICompatible);
/// Shorthand for registry entries: Gemini listing discovery.
pub const GEMINI_MODELS: ModelDiscovery = ModelDiscovery::Supported(DiscoveryProtocol::GeminiList);

/// Per-model discovery resolver stored on
/// [`crate::registry::ProviderEntry::model_discovery_for_model`], for families
/// that pick the wire protocol by MODEL NAME (r9s serves `claude-*` over the
/// Anthropic Messages API). Given the selected model and the configured base
/// URL (`None` = the family default root), returns the listing strategy and,
/// when that strategy speaks against a DIFFERENT root than the configured
/// one, that root — mirroring the rewrite the family's `create` applies.
pub type ModelDiscoveryForModel =
    fn(model: &str, base_url: Option<&str>) -> (ModelDiscovery, Option<String>);

/// The resolved discovery route: which listing strategy to speak AND, for
/// per-model-protocol families, the API root to speak it against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryRoute {
    /// The listing strategy the probe must use.
    pub discovery: ModelDiscovery,
    /// Rewritten API root from a per-model resolver (r9s `claude-*` →
    /// `{base}/anthropic`). When set it WINS over the caller's base_url and
    /// the family default — it is derived from them, never a third source.
    pub base_url: Option<String>,
}

impl From<ModelDiscovery> for DiscoveryRoute {
    /// A plain family-wide declaration routes with no root rewrite.
    fn from(discovery: ModelDiscovery) -> Self {
        DiscoveryRoute {
            discovery,
            base_url: None,
        }
    }
}

/// The typed result of one discovery attempt. Every failure carries a safe,
/// redacted message (never a credential) so callers can show it verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryOutcome {
    /// The endpoint answered with a model catalog (possibly empty — an empty
    /// successful catalog is NOT a failure and stays distinguishable).
    Discovered(Vec<String>),
    /// No model-list endpoint: declared unsupported by the family, or the
    /// endpoint answered 404. Manual model-id entry remains the path.
    Unsupported(String),
    /// The credential was rejected (HTTP 401/403).
    AuthenticationFailed(String),
    /// The endpoint could not be reached (transport error, timeout, no API
    /// root) or answered with another non-success status.
    EndpointUnreachable(String),
    /// The endpoint answered but the body was not a valid catalog.
    InvalidResponse(String),
    /// The provider rate-limited discovery (HTTP 429).
    RateLimited(String),
}

impl DiscoveryOutcome {
    /// Stable machine-readable label shared by the AppUI RPC and the admin
    /// REST surface (single mapping — the two clients cannot drift).
    pub fn status_label(&self) -> &'static str {
        match self {
            DiscoveryOutcome::Discovered(_) => "discovered",
            DiscoveryOutcome::Unsupported(_) => "unsupported",
            DiscoveryOutcome::AuthenticationFailed(_) => "authentication_failed",
            DiscoveryOutcome::EndpointUnreachable(_) => "endpoint_unreachable",
            DiscoveryOutcome::InvalidResponse(_) => "invalid_response",
            DiscoveryOutcome::RateLimited(_) => "rate_limited",
        }
    }

    /// The discovered model ids; `None` when discovery did not succeed.
    pub fn models(&self) -> Option<&[String]> {
        match self {
            DiscoveryOutcome::Discovered(models) => Some(models),
            _ => None,
        }
    }

    /// The safe, redacted human-readable message for non-`discovered`
    /// outcomes; `None` on success.
    pub fn message(&self) -> Option<&str> {
        match self {
            DiscoveryOutcome::Discovered(_) => None,
            DiscoveryOutcome::Unsupported(message)
            | DiscoveryOutcome::AuthenticationFailed(message)
            | DiscoveryOutcome::EndpointUnreachable(message)
            | DiscoveryOutcome::InvalidResponse(message)
            | DiscoveryOutcome::RateLimited(message) => Some(message),
        }
    }

    /// Whether this outcome means "discovery could not help, but inference
    /// and manual model-id entry are unaffected" (advisory failure).
    pub fn is_advisory(&self) -> bool {
        matches!(self, DiscoveryOutcome::Unsupported(_))
    }
}

/// Resolve the discovery route for a selected route + model.
///
/// Mirrors `create_provider_with_api_type`'s precedence exactly: an explicit
/// `api_type: "anthropic"` overrides any family (the runtime bypasses the
/// registry for that case), and for every other `api_type` the registered
/// family's own factory — hence its declared discovery — rules. Unknown or
/// `custom` families fall back to the OpenAI-compatible strategy, matching the
/// runtime's custom-provider default. The protocol is never inferred from a
/// single literal family id.
///
/// Families that select the wire protocol by MODEL NAME (r9s: `claude-*` →
/// Anthropic Messages at a rewritten `{base}/anthropic` root) carry a
/// per-model resolver on their registry entry; a selected model is resolved
/// through it so the probe follows the same protocol AND root the provider
/// factory would use. No model yet (listing before selection) uses the
/// family-wide declaration.
pub fn resolve_model_discovery(
    family: Option<&str>,
    api_type: Option<&str>,
    model: Option<&str>,
    base_url: Option<&str>,
) -> DiscoveryRoute {
    if api_type == Some("anthropic") {
        return ANTHROPIC_MODELS.into();
    }
    // Normalize like `discover_models` does before handing the base URL to a
    // per-model resolver — an empty/whitespace override must fall through to
    // the family default root, never reach the resolver as `Some("")`.
    let base_url = base_url.map(str::trim).filter(|root| !root.is_empty());
    match family
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .and_then(crate::registry::lookup)
    {
        Some(entry) => {
            // The model is matched RAW (no trim), exactly like provider
            // construction's `prefers_anthropic` — a whitespace-padded model
            // is served OpenAI at runtime, so discovery must not diverge.
            let model = model.filter(|m| !m.is_empty());
            if let (Some(resolver), Some(model)) = (entry.model_discovery_for_model, model) {
                let (discovery, base_url) = resolver(model, base_url);
                return DiscoveryRoute {
                    discovery,
                    base_url,
                };
            }
            entry.model_discovery.into()
        }
        // Unknown family / `custom` without an anthropic override: the runtime
        // builds an OpenAI-compatible provider, so discovery matches that.
        None => OPENAI_MODELS.into(),
    }
}

/// Join the models-listing URL relative to the configured API root.
///
/// The root is the API root exactly as providers' own default base URLs spell
/// it (`https://api.openai.com/v1`, `https://open.bigmodel.cn/api/paas/v4`,
/// `https://api.z.ai/api/anthropic`); the strategy appends only its declared
/// path. No version segments are stripped or re-added heuristically, so a
/// `/v4` root can never turn into `/v4/v1/models`.
pub fn models_url(root: &str, protocol: DiscoveryProtocol) -> String {
    let root = root.trim().trim_end_matches('/');
    match protocol {
        DiscoveryProtocol::AnthropicMessages => format!("{root}/v1/models"),
        DiscoveryProtocol::OpenAICompatible | DiscoveryProtocol::GeminiList => {
            format!("{root}/models")
        }
    }
}

/// Run one discovery attempt against the resolved route.
///
/// `api_key` may be empty for keyless families — the auth header is then
/// suppressed rather than sent as a literal `Bearer ` (mirroring the
/// connection-test path). The route's rewritten root (per-model families)
/// wins, then `base_url`, then the family's registered default root. Never
/// includes the credential in any returned message.
pub async fn discover_models(
    route: &DiscoveryRoute,
    api_key: &str,
    base_url: Option<&str>,
    family: Option<&str>,
) -> DiscoveryOutcome {
    let protocol = match route.discovery {
        ModelDiscovery::Unsupported(reason) => {
            return DiscoveryOutcome::Unsupported(reason.to_string());
        }
        ModelDiscovery::Supported(protocol) => protocol,
    };
    let root = route
        .base_url
        .clone()
        .or_else(|| {
            base_url
                .map(str::trim)
                .filter(|root| !root.is_empty())
                .map(str::to_string)
        })
        .or_else(|| {
            family
                .map(str::trim)
                .filter(|family| !family.is_empty())
                .and_then(crate::registry::lookup)
                .and_then(|entry| entry.default_base_url)
                .map(str::to_string)
        });
    let Some(root) = root else {
        return DiscoveryOutcome::EndpointUnreachable(
            "no API root configured for model discovery — set base_url or enter the model id \
             manually"
                .into(),
        );
    };
    let url = models_url(&root, protocol);
    let mut request = reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(10));
    match protocol {
        DiscoveryProtocol::AnthropicMessages => {
            request = request
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01");
        }
        DiscoveryProtocol::GeminiList => {
            if !api_key.is_empty() {
                request = request.header("x-goog-api-key", api_key);
            }
        }
        DiscoveryProtocol::OpenAICompatible => {
            // Keyless families pass an empty key — suppress the header instead
            // of sending a literal `Bearer ` (mirrors test_provider).
            if !api_key.is_empty() {
                request = request.header("Authorization", format!("Bearer {api_key}"));
            }
        }
    }

    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            return DiscoveryOutcome::EndpointUnreachable(format!(
                "model listing at {url} is unreachable: {error}"
            ));
        }
    };
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return DiscoveryOutcome::AuthenticationFailed(format!(
            "the provider rejected the credential for model discovery (HTTP {status}) — check the \
             API key"
        ));
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return DiscoveryOutcome::RateLimited(format!(
            "the provider rate-limited model discovery (HTTP {status}) — retry later"
        ));
    }
    if status == reqwest::StatusCode::NOT_FOUND {
        // A discovery-only 404 is advisory: inference can be perfectly fine
        // while the family publishes no model-list endpoint.
        return DiscoveryOutcome::Unsupported(format!(
            "no model-list endpoint at {url} (HTTP 404) — enter the model id manually; inference \
             is unaffected"
        ));
    }
    if !status.is_success() {
        return DiscoveryOutcome::EndpointUnreachable(format!(
            "model listing at {url} returned HTTP {status}"
        ));
    }
    let body = match response.text().await {
        Ok(body) => body,
        Err(error) => {
            return DiscoveryOutcome::InvalidResponse(format!(
                "reading the model-listing response failed: {error}"
            ));
        }
    };
    let parsed: serde_json::Value = match serde_json::from_str(&body) {
        Ok(parsed) => parsed,
        Err(_) => {
            return DiscoveryOutcome::InvalidResponse(
                "model listing returned a non-JSON body".into(),
            );
        }
    };
    let mut ids = match protocol {
        DiscoveryProtocol::AnthropicMessages | DiscoveryProtocol::OpenAICompatible => parsed
            .get("data")
            .and_then(serde_json::Value::as_array)
            .map(|models| {
                models
                    .iter()
                    .filter_map(|model| model.get("id").and_then(serde_json::Value::as_str))
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            }),
        DiscoveryProtocol::GeminiList => parsed
            .get("models")
            .and_then(serde_json::Value::as_array)
            .map(|models| {
                models
                    .iter()
                    .filter_map(|model| model.get("name").and_then(serde_json::Value::as_str))
                    .map(|name| {
                        // Gemini names entries "models/gemini-2.0-flash";
                        // surface the bare model id callers configure.
                        name.strip_prefix("models/").unwrap_or(name).to_string()
                    })
                    .collect::<Vec<_>>()
            }),
    };
    match ids.as_mut() {
        Some(ids) => {
            ids.sort();
            ids.dedup();
            DiscoveryOutcome::Discovered(std::mem::take(ids))
        }
        None => DiscoveryOutcome::InvalidResponse(format!(
            "model listing at {url} returned an unexpected response shape"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Strategy resolution ─────────────────────────────────────────────

    #[test]
    fn should_use_native_anthropic_strategy_for_zai_family_without_literal_id_match() {
        // The registry entry — not a `provider == "anthropic"` literal —
        // declares the protocol, so the Anthropic-protocol zai family resolves
        // to the Anthropic Messages strategy even though its id differs.
        assert_eq!(
            resolve_model_discovery(Some("zai"), None, None, None).discovery,
            ANTHROPIC_MODELS
        );
        assert_eq!(
            resolve_model_discovery(Some("zai-coding"), None, None, None).discovery,
            ANTHROPIC_MODELS
        );
    }

    #[test]
    fn should_resolve_family_alias_to_the_same_strategy_as_the_canonical_family() {
        for alias in ["z.ai", "z.ai-coding", "glm-coding"] {
            let canonical = match alias {
                "z.ai" => "zai",
                _ => "zai-coding",
            };
            assert_eq!(
                resolve_model_discovery(Some(alias), None, None, None),
                resolve_model_discovery(Some(canonical), None, None, None),
                "alias {alias} must resolve like {canonical}"
            );
        }
    }

    #[test]
    fn should_apply_route_api_type_anthropic_override_over_the_native_strategy() {
        // The runtime's `create_provider_with_api_type` bypasses the registry
        // for `api_type: "anthropic"`; discovery mirrors that override.
        assert_eq!(
            resolve_model_discovery(Some("openai"), Some("anthropic"), None, None).discovery,
            ANTHROPIC_MODELS
        );
        // And a registered Anthropic-protocol family stays on-strategy.
        assert_eq!(
            resolve_model_discovery(Some("zai"), Some("anthropic"), None, None).discovery,
            ANTHROPIC_MODELS
        );
    }

    #[test]
    fn should_keep_native_strategy_when_api_type_is_not_the_anthropic_override() {
        // Mirrors inference: for registered families only `api_type:
        // "anthropic"` changes protocol — "openai" on zai still constructs
        // AnthropicProvider, so discovery must NOT take the bait either
        // (saved AppUI routes default api_type to "openai").
        assert_eq!(
            resolve_model_discovery(Some("zai"), Some("openai"), None, None).discovery,
            ANTHROPIC_MODELS
        );
        assert_eq!(
            resolve_model_discovery(Some("zhipu"), Some("openai"), None, None).discovery,
            OPENAI_MODELS
        );
        assert_eq!(
            resolve_model_discovery(Some("anthropic"), Some("openai"), None, None).discovery,
            // The literal anthropic family's native protocol is Messages, but
            // its route explicitly says the openai override… which for a
            // registered family is ignored at runtime — native rules.
            ANTHROPIC_MODELS
        );
    }

    #[test]
    fn should_fall_back_to_openai_strategy_for_unknown_and_custom_families() {
        assert_eq!(
            resolve_model_discovery(Some("custom"), None, None, None).discovery,
            OPENAI_MODELS
        );
        assert_eq!(
            resolve_model_discovery(Some("custom"), Some("openai"), None, None).discovery,
            OPENAI_MODELS
        );
        assert_eq!(
            resolve_model_discovery(Some("custom"), Some("anthropic"), None, None).discovery,
            ANTHROPIC_MODELS
        );
        assert_eq!(
            resolve_model_discovery(Some("not-a-family"), None, None, None).discovery,
            OPENAI_MODELS
        );
        assert_eq!(
            resolve_model_discovery(None, None, None, None).discovery,
            OPENAI_MODELS
        );
    }

    #[test]
    fn should_declare_unsupported_discovery_for_vertex() {
        let discovery = resolve_model_discovery(Some("vertex"), None, None, None);
        match discovery.discovery {
            ModelDiscovery::Unsupported(reason) => {
                assert!(
                    reason.contains("manually"),
                    "reason must point at manual entry"
                );
            }
            other => panic!("vertex must be manual-only, got {other:?}"),
        }
    }

    // ── URL joining ─────────────────────────────────────────────────────

    #[test]
    fn should_join_openai_models_relative_to_the_api_root_without_version_duplication() {
        // The /v4 regression: a versioned root must never become /v4/v1/models.
        assert_eq!(
            models_url(
                "https://open.bigmodel.cn/api/paas/v4",
                DiscoveryProtocol::OpenAICompatible
            ),
            "https://open.bigmodel.cn/api/paas/v4/models"
        );
        assert_eq!(
            models_url(
                "https://api.openai.com/v1/",
                DiscoveryProtocol::OpenAICompatible
            ),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            models_url(
                "https://my-gateway.example.com",
                DiscoveryProtocol::OpenAICompatible
            ),
            "https://my-gateway.example.com/models"
        );
    }

    #[test]
    fn should_join_anthropic_models_relative_to_the_api_root() {
        assert_eq!(
            models_url(
                "https://api.z.ai/api/anthropic",
                DiscoveryProtocol::AnthropicMessages
            ),
            "https://api.z.ai/api/anthropic/v1/models"
        );
        assert_eq!(
            models_url(
                "https://api.anthropic.com",
                DiscoveryProtocol::AnthropicMessages
            ),
            "https://api.anthropic.com/v1/models"
        );
    }

    // ── HTTP executor against a loopback fixture ────────────────────────

    /// One captured request: path + auth-relevant headers.
    struct Captured {
        path: String,
        authorization: Option<String>,
        x_api_key: Option<String>,
    }

    /// Raw-TCP loopback fixture: serves a scripted response for every
    /// connection and records each request. One accepted connection per
    /// `Connection: close` request keeps the protocol trivially correct.
    async fn spawn_fixture(
        status_line: &'static str,
        body: &'static str,
    ) -> (String, std::sync::Arc<tokio::sync::Mutex<Vec<Captured>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let captured = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let recorded = captured.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                // Read until the END OF HEADERS — a single read can return a
                // partial request, and answering before the client finished
                // sending makes the close reset the connection mid-request.
                let mut raw = Vec::new();
                let mut chunk = [0_u8; 2048];
                loop {
                    match socket.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            raw.extend_from_slice(&chunk[..read]);
                            if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                let request = String::from_utf8_lossy(&raw).to_string();
                let mut lines = request.split("\r\n");
                let request_line = lines.next().unwrap_or_default().to_string();
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                let header = |name: &str| -> Option<String> {
                    lines.clone().find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case(name)
                            .then(|| value.trim().to_string())
                    })
                };
                recorded.lock().await.push(Captured {
                    path,
                    authorization: header("authorization"),
                    x_api_key: header("x-api-key"),
                });
                let response = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: \
                     {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        (format!("http://{address}"), captured)
    }

    #[tokio::test]
    async fn should_probe_anthropic_protocol_for_zai_without_bearer_or_v1_models() {
        let (root, captured) =
            spawn_fixture("200 OK", r#"{"data":[{"id":"glm-4.7"},{"id":"glm-5.2"}]}"#).await;
        // Base root exactly as the zai family spells it (no /v1 suffix).
        let url = format!("{root}/api/anthropic");
        let outcome = discover_models(
            &resolve_model_discovery(Some("zai"), Some("openai"), None, None),
            "zai-key-secret",
            Some(&url),
            Some("zai"),
        )
        .await;

        assert_eq!(outcome.status_label(), "discovered");
        assert_eq!(
            outcome.models(),
            Some(vec!["glm-4.7".to_string(), "glm-5.2".to_string()].as_slice())
        );
        let requests = captured.lock().await;
        assert_eq!(requests.len(), 1);
        let only = &requests[0];
        // The strategy-derived path, NOT a synthesized /v1/models off the
        // root, and Anthropic header semantics — never `Authorization: Bearer`.
        assert_eq!(only.path, "/api/anthropic/v1/models");
        assert!(
            only.authorization.is_none(),
            "zai must never get a Bearer probe"
        );
        assert_eq!(only.x_api_key.as_deref(), Some("zai-key-secret"));
    }

    #[tokio::test]
    async fn should_not_duplicate_version_segments_for_versioned_openai_roots() {
        let (root, captured) = spawn_fixture("200 OK", r#"{"data":[{"id":"glm-5.2"}]}"#).await;
        let url = format!("{root}/api/paas/v4");
        let outcome = discover_models(
            &resolve_model_discovery(Some("zhipu"), None, None, None),
            "zhipu-key",
            Some(&url),
            Some("zhipu"),
        )
        .await;

        assert_eq!(outcome.status_label(), "discovered");
        let requests = captured.lock().await;
        assert_eq!(requests[0].path, "/api/paas/v4/models");
        assert_ne!(requests[0].path, "/api/paas/v4/v1/models");
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some("Bearer zhipu-key")
        );
    }

    #[tokio::test]
    async fn should_map_401_to_authentication_failed_and_404_to_unsupported() {
        let (root, _) = spawn_fixture("401 Unauthorized", r#"{"error":{}}"#).await;
        let outcome = discover_models(
            &OPENAI_MODELS.into(),
            "bad-key",
            Some(&format!("{root}/v1")),
            Some("openai"),
        )
        .await;
        assert_eq!(outcome.status_label(), "authentication_failed");

        let (root, _) = spawn_fixture("404 Not Found", "nope").await;
        let outcome = discover_models(
            &OPENAI_MODELS.into(),
            "k",
            Some(&format!("{root}/v1")),
            Some("openai"),
        )
        .await;
        assert_eq!(outcome.status_label(), "unsupported");
        assert!(outcome.is_advisory());
        assert!(outcome.message().is_some_and(|m| m.contains("manually")));
    }

    #[tokio::test]
    async fn should_map_rate_limit_transport_and_malformed_responses_distinctly() {
        let (root, _) = spawn_fixture("429 Too Many Requests", "{}").await;
        let outcome = discover_models(
            &OPENAI_MODELS.into(),
            "k",
            Some(&format!("{root}/v1")),
            Some("openai"),
        )
        .await;
        assert_eq!(outcome.status_label(), "rate_limited");

        // Nothing listens on port 1 — transport failure, not an auth problem.
        let outcome = discover_models(
            &OPENAI_MODELS.into(),
            "k",
            Some("http://127.0.0.1:1/v1"),
            Some("openai"),
        )
        .await;
        assert_eq!(outcome.status_label(), "endpoint_unreachable");

        let (root, _) = spawn_fixture("200 OK", "<html>not json</html>").await;
        let outcome = discover_models(
            &OPENAI_MODELS.into(),
            "k",
            Some(&format!("{root}/v1")),
            Some("openai"),
        )
        .await;
        assert_eq!(outcome.status_label(), "invalid_response");

        // Shape mismatch: 200 + JSON without the expected array.
        let (root, _) = spawn_fixture("200 OK", r#"{"models":"bogus"}"#).await;
        let outcome = discover_models(
            &OPENAI_MODELS.into(),
            "k",
            Some(&format!("{root}/v1")),
            Some("openai"),
        )
        .await;
        assert_eq!(outcome.status_label(), "invalid_response");
    }

    #[tokio::test]
    async fn should_keep_empty_successful_catalogs_distinguishable_from_failures() {
        let (root, _) = spawn_fixture("200 OK", r#"{"data":[]}"#).await;
        let outcome = discover_models(
            &OPENAI_MODELS.into(),
            "k",
            Some(&format!("{root}/v1")),
            Some("openai"),
        )
        .await;
        assert_eq!(outcome, DiscoveryOutcome::Discovered(Vec::new()));
        assert_eq!(outcome.status_label(), "discovered");
        assert!(outcome.message().is_none());
    }

    #[tokio::test]
    async fn should_return_declared_unsupported_without_any_outbound_request() {
        let (root, captured) = spawn_fixture("200 OK", r#"{"data":[]}"#).await;
        let outcome = discover_models(
            &resolve_model_discovery(Some("vertex"), None, None, None),
            "sa-json",
            Some(&root),
            Some("vertex"),
        )
        .await;
        assert_eq!(outcome.status_label(), "unsupported");
        assert!(outcome.models().is_none());
        assert!(
            outcome.message().is_some_and(|m| m.contains("manually")),
            "unsupported must explain the manual path"
        );
        assert!(
            captured.lock().await.is_empty(),
            "manual-only families must never be probed"
        );
    }

    /// r9s picks its wire protocol by model name (`claude-*` → Anthropic
    /// Messages at `{base}/anthropic`); the discovery route must follow the
    /// selected model, not the family-wide OpenAI declaration (octos#2185).
    #[test]
    fn should_resolve_r9s_discovery_per_model() {
        // claude-* → Anthropic strategy against the rewritten root, derived
        // from the configured base URL when one is set…
        let route = resolve_model_discovery(
            Some("r9s"),
            None,
            Some("claude-sonnet-4"),
            Some("https://proxy.example.com/v1"),
        );
        assert_eq!(route.discovery, ANTHROPIC_MODELS);
        assert_eq!(
            route.base_url.as_deref(),
            Some("https://proxy.example.com/anthropic")
        );
        // …and from the family default root when none is.
        let route = resolve_model_discovery(Some("r9s"), None, Some("claude-sonnet-4"), None);
        assert_eq!(route.discovery, ANTHROPIC_MODELS);
        assert_eq!(
            route.base_url.as_deref(),
            Some("https://api.r9s.ai/anthropic")
        );

        // An empty/whitespace base_url override normalizes away BEFORE the
        // resolver sees it — `Some("")` must never rewrite to "/anthropic"
        // and strand the probe on a relative URL.
        for base in [Some(""), Some("   ")] {
            let route = resolve_model_discovery(Some("r9s"), None, Some("claude-sonnet-4"), base);
            assert_eq!(
                route.base_url.as_deref(),
                Some("https://api.r9s.ai/anthropic"),
                "empty base_url {base:?} must fall back to the family default root"
            );
        }
        // A padded base_url is trimmed so the `/v1` suffix strip still lands.
        let route = resolve_model_discovery(
            Some("r9s"),
            None,
            Some("claude-sonnet-4"),
            Some("  https://proxy.example.com/v1  "),
        );
        assert_eq!(
            route.base_url.as_deref(),
            Some("https://proxy.example.com/anthropic")
        );

        // A non-claude model keeps the family-wide OpenAI listing with no
        // root rewrite — as does no model yet (listing before selection).
        // Models are matched RAW like `create` does: a whitespace-padded
        // model is served OpenAI at runtime, so discovery must not diverge.
        for model in [
            Some("gpt-5"),
            Some("Claude-sonnet-4"),
            Some(" claude-sonnet-4"),
            None,
            Some(""),
        ] {
            let route = resolve_model_discovery(Some("r9s"), None, model, None);
            assert_eq!(
                route,
                OPENAI_MODELS.into(),
                "model {model:?} must use the family-wide OpenAI route"
            );
        }

        // The explicit api_type override still wins over per-model resolution,
        // exactly like it bypasses the registry at runtime.
        let route = resolve_model_discovery(Some("r9s"), Some("anthropic"), Some("gpt-5"), None);
        assert_eq!(route.discovery, ANTHROPIC_MODELS);
    }

    /// The r9s claude probe must hit the Anthropic listing at the rewritten
    /// root with Anthropic header semantics — never `{base}/models` with a
    /// Bearer header (octos#2185).
    #[tokio::test]
    async fn should_probe_anthropic_root_for_r9s_claude_selection_without_bearer() {
        let (root, captured) =
            spawn_fixture("200 OK", r#"{"data":[{"id":"claude-sonnet-4"}]}"#).await;
        // Configured base exactly as the r9s family spells it (`{base}/v1`).
        let url = format!("{root}/v1");
        let outcome = discover_models(
            &resolve_model_discovery(Some("r9s"), None, Some("claude-sonnet-4"), Some(&url)),
            "r9s-key",
            Some(&url),
            Some("r9s"),
        )
        .await;

        assert_eq!(outcome.status_label(), "discovered");
        let requests = captured.lock().await;
        assert_eq!(requests.len(), 1);
        let only = &requests[0];
        assert_eq!(only.path, "/anthropic/v1/models");
        assert!(
            only.authorization.is_none(),
            "r9s claude-* must never get a Bearer probe"
        );
        assert_eq!(only.x_api_key.as_deref(), Some("r9s-key"));
    }

    #[test]
    fn should_build_gemini_models_url_relative_to_the_versioned_root() {
        assert_eq!(
            models_url(
                "https://generativelanguage.googleapis.com/v1beta",
                DiscoveryProtocol::GeminiList
            ),
            "https://generativelanguage.googleapis.com/v1beta/models"
        );
    }
}
