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
//! Anthropic Messages strategy, an `api_type` of `responses` the
//! OpenAI-compatible listing, everything else follows the family's declared
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
/// Mirrors `create_provider_with_api_type`'s precedence exactly: explicit
/// `api_type` overrides that bypass the registry at runtime — `"anthropic"`
/// (Anthropic Messages at the route's root) and `"responses"` (OpenAI
/// Responses at `{base}`) — override any family here too, and for every other
/// `api_type` the registered family's own factory — hence its declared
/// discovery — rules. Unknown or `custom` families fall back to the
/// OpenAI-compatible strategy, matching the runtime's custom-provider
/// default. The protocol is never inferred from a single literal family id.
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
    let entry = family
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .and_then(crate::registry::lookup);
    let saved_base_url = base_url.map(str::trim).filter(|root| !root.is_empty());
    if api_type == Some("anthropic") {
        // No base_url: probe the root the runtime's anthropic arm targets,
        // which for z.ai is not the family's (OpenAI) default root.
        let base_url = match (saved_base_url, entry) {
            (None, Some(entry)) => crate::registry::anthropic_api_type_default_root(entry)
                .filter(|root| Some(*root) != entry.default_base_url)
                .map(str::to_string),
            _ => None,
        };
        return DiscoveryRoute {
            discovery: ANTHROPIC_MODELS,
            base_url,
        };
    }
    // api_type "responses" bypasses the registry at runtime
    // (OpenAIResponsesProvider at {base}), so the listing to probe is the
    // OpenAI-compatible `GET {base}/models` — never the family's declared
    // (possibly Anthropic) strategy, and never a per-model resolver's root
    // rewrite. A family that declares discovery Unsupported (vertex) stays
    // manual-only: no protocol override conjures up a listing endpoint it
    // does not have.
    if api_type == Some("responses") {
        return match entry {
            Some(entry) => match entry.model_discovery {
                unsupported @ ModelDiscovery::Unsupported(_) => unsupported.into(),
                ModelDiscovery::Supported(_) => OPENAI_MODELS.into(),
            },
            None => OPENAI_MODELS.into(),
        };
    }
    // Normalize like `discover_models` does before handing the base URL to a
    // per-model resolver — an empty/whitespace override must fall through to
    // the family default root, never reach the resolver as `Some("")`.
    let base_url = base_url.map(str::trim).filter(|root| !root.is_empty());
    match entry {
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
            // A saved root the family's factory migrates (z.ai's legacy
            // Anthropic root) is probed where inference actually goes.
            DiscoveryRoute {
                discovery: entry.model_discovery,
                base_url: base_url
                    .and_then(|root| crate::registry::migrated_lane_base_url(entry, root)),
            }
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
/// `https://api.z.ai/api/paas/v4`); the strategy appends only its declared
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

    // ── URL joining ─────────────────────────────────────────────────────

    // ── HTTP executor against a loopback fixture ────────────────────────

    struct Captured;

    /// Raw-TCP loopback fixture: serves a scripted response for every
    /// connection and counts each request. One accepted connection per
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
                recorded.lock().await.push(Captured);
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
}
