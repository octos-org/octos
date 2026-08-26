//! Probe a local server for its actual context window.
//!
//! The catalog rows for local families carry deliberately modest
//! `context_window` guesses (32K for `local`, similar for `ollama`/`vllm`)
//! because at registration time nothing knows what the operator launched.
//! But the running server DOES know: llama.cpp's `/props` reports the `-c`
//! value it was started with, and every OpenAI-compatible engine exposes
//! some spelling of the window on `GET /v1/models` (see
//! [`crate::local_discovery`]). Budgeting a 256K server as 32K is not a safe
//! under-estimate — the compaction loop shreds the working set to fit the
//! phantom limit, and long tasks degrade into re-read thrash (observed: a
//! 1,182-line source file read 66 times in one session while the live
//! context sat at ~19K of an actual 256K).
//!
//! [`LocalContextProbe`] wraps a local-family provider. The probe runs in
//! the background, spawned at construction when a runtime is available (so
//! a resumed long session gets the corrected window before its first
//! compaction pass, not after its first send), and re-attempted from the
//! request path otherwise. Outcomes are handled by kind, not collapsed:
//!
//! - the server ANSWERED and named a window → pinned;
//! - the server ANSWERED both endpoints without naming one → pinned as
//!   "no window", the catalog value stands (re-asking will not change it);
//! - the server was UNREACHABLE or mid-load (5xx, timeout) → NOT pinned:
//!   the next request retries, up to [`MAX_PROBE_ATTEMPTS`], because "the
//!   model was still loading during the first message" is precisely the
//!   session that needs the correction later.
//!
//! `context_window()` is sync and never blocks: the inner (catalog) value
//! before the probe lands, the server's truth after. Probe requests carry
//! the configured API key (llama-server / vLLM `--api-key` deployments are
//! exactly the ones that would otherwise 401) and honor the configured HTTP
//! connect timeout (a GPU box over a slow tunnel is exactly the deployment
//! most likely to run a large `-c`).

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;
use eyre::Result;
use octos_core::Message;

use crate::config::ChatConfig;
use crate::local_discovery::{
    parse_models_context_window, parse_ollama_ps_context_window, parse_props_context_window,
};
use crate::provider::{LlmProvider, build_http_client};
use crate::types::{ChatResponse, ChatStream, ProviderMetadata, ToolSpec};

/// Default probe timeouts: a local server answers these endpoints in
/// milliseconds; these apply only when no HTTP timeout was configured.
const DEFAULT_PROBE_TIMEOUT_SECS: u64 = 3;
const DEFAULT_PROBE_CONNECT_TIMEOUT_SECS: u64 = 2;
/// The probe can ride in front of a user request when it runs inline, so
/// even a generous configured request timeout is capped here.
const MAX_PROBE_TIMEOUT_SECS: u64 = 15;
/// Transient failures retry on later requests, but a server that never
/// answers the probe endpoints must not cost failed GETs per request
/// forever.
const MAX_PROBE_ATTEMPTS: u32 = 5;

/// llama.cpp serves `/props` at the server root, not under `/v1`.
/// Single-suffix strip: a proxy mounting the API under `/v1/v1` keeps its
/// first `/v1` as the server root (`trim_end_matches` would strip every
/// repetition and 404 the probe).
fn props_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    let root = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    format!("{root}/props")
}

/// The OpenAI list-models endpoint, relative to the configured base.
fn models_url(base_url: &str) -> String {
    format!("{}/models", base_url.trim_end_matches('/'))
}

/// Ollama's native process-status endpoint, at the server root.
fn ollama_ps_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    let root = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    format!("{root}/api/ps")
}

/// What one probe GET established — the distinction the retry logic runs on.
enum FetchOutcome {
    /// The server answered decisively: 2xx with a body, or a definitive
    /// client-side status (404, 401) that re-asking will not change.
    Answered(Option<String>),
    /// Transport error, timeout, or 5xx: the server may simply not be up
    /// yet (a large model still loading answers exactly like this).
    Transient,
}

/// Which endpoints the probe asks — the engines disagree (#2135 review:
/// Ollama serves no `/props`, and its OpenAI-compatible model list carries
/// no context length; its allocated window lives on the native `/api/ps`).
enum ProbeEndpoints {
    /// llama.cpp / vLLM / LM Studio: `/props` (authoritative) then
    /// `{base}/models`.
    OpenAiCompatible {
        props_url: String,
        models_url: String,
    },
    /// Ollama: `GET /api/ps` — the RUNNING models with their allocated
    /// `context_length`.
    OllamaNative { ps_url: String },
}

/// Wraps a local-family provider; overrides `context_window()` with the
/// server-reported value once the probe has resolved.
pub struct LocalContextProbe {
    inner: Arc<dyn LlmProvider>,
    endpoints: ProbeEndpoints,
    api_key: Option<String>,
    timeouts: (u64, u64),
    /// Set ONLY on a definitive outcome: `Some(w)` = the server named its
    /// window; `None` = the server answered but names none (or the attempt
    /// cap was spent) — the catalog value stands either way.
    window: OnceLock<Option<u32>>,
    attempts: AtomicU32,
    in_flight: tokio::sync::Mutex<()>,
}

impl LocalContextProbe {
    /// Wrap `inner` and start probing in the background if a runtime is
    /// available (gateway construction runs inside one; the fallback is the
    /// request path). `api_key` is the key the wrapped provider itself
    /// authenticates with; `http_timeout` is the configured
    /// `(request, connect)` override, if any.
    pub fn new(
        inner: Arc<dyn LlmProvider>,
        base_url: &str,
        api_key: Option<String>,
        http_timeout: Option<(u64, u64)>,
    ) -> Arc<Self> {
        let timeouts = http_timeout
            .map(|(t, c)| (t.min(MAX_PROBE_TIMEOUT_SECS), c))
            .unwrap_or((
                DEFAULT_PROBE_TIMEOUT_SECS,
                DEFAULT_PROBE_CONNECT_TIMEOUT_SECS,
            ));
        Self::with_endpoints(
            inner,
            ProbeEndpoints::OpenAiCompatible {
                props_url: props_url(base_url),
                models_url: models_url(base_url),
            },
            api_key,
            timeouts,
        )
    }

    /// Ollama-native probing: `GET /api/ps` (allocated context of the
    /// running models). Ollama takes no API key.
    pub fn new_ollama(
        inner: Arc<dyn LlmProvider>,
        base_url: &str,
        http_timeout: Option<(u64, u64)>,
    ) -> Arc<Self> {
        let timeouts = http_timeout
            .map(|(t, c)| (t.min(MAX_PROBE_TIMEOUT_SECS), c))
            .unwrap_or((
                DEFAULT_PROBE_TIMEOUT_SECS,
                DEFAULT_PROBE_CONNECT_TIMEOUT_SECS,
            ));
        Self::with_endpoints(
            inner,
            ProbeEndpoints::OllamaNative {
                ps_url: ollama_ps_url(base_url),
            },
            None,
            timeouts,
        )
    }

    fn with_endpoints(
        inner: Arc<dyn LlmProvider>,
        endpoints: ProbeEndpoints,
        api_key: Option<String>,
        timeouts: (u64, u64),
    ) -> Arc<Self> {
        let probe = Arc::new(Self {
            inner,
            endpoints,
            api_key: api_key.filter(|k| !k.is_empty() && k != "no-key"),
            timeouts,
            window: OnceLock::new(),
            attempts: AtomicU32::new(0),
            in_flight: tokio::sync::Mutex::new(()),
        });
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let background = probe.clone();
            handle.spawn(async move { background.probe_if_unresolved().await });
        }
        probe
    }

    /// Run one probe attempt unless the outcome is already pinned, another
    /// attempt is in flight (skip, don't queue — the request path must not
    /// stack behind a slow probe), or the attempt cap is spent.
    async fn probe_if_unresolved(&self) {
        if self.window.get().is_some() {
            return;
        }
        let Ok(_guard) = self.in_flight.try_lock() else {
            return;
        };
        if self.window.get().is_some() {
            return;
        }
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt > MAX_PROBE_ATTEMPTS {
            // Spent: stop paying failed GETs on every request. Pinning "no
            // window" keeps the catalog value without further probing.
            let _ = self.window.set(None);
            return;
        }

        let client = build_http_client(self.timeouts.0, self.timeouts.1);
        let (window, all_answered) = match &self.endpoints {
            ProbeEndpoints::OpenAiCompatible {
                props_url,
                models_url,
            } => {
                // Both GETs concurrently — the probe may be riding in front
                // of a user-visible request, so the failure worst-case must
                // be one timeout, not two in sequence.
                let (props, models) = tokio::join!(
                    fetch(&client, props_url, self.api_key.as_deref()),
                    fetch(&client, models_url, self.api_key.as_deref()),
                );
                // `/props` wins when it names a window: it reports the value
                // the server was LAUNCHED with, which caps whatever the
                // per-model metadata claims (a model trained for 256K served
                // with -c 32768 really has 32K).
                let window = match (&props, &models) {
                    (FetchOutcome::Answered(Some(body)), _)
                        if parse_props_context_window(body).is_some() =>
                    {
                        parse_props_context_window(body)
                    }
                    (_, FetchOutcome::Answered(Some(body))) => {
                        parse_models_context_window(body, self.inner.model_id())
                    }
                    _ => None,
                };
                let all_answered = matches!(props, FetchOutcome::Answered(_))
                    && matches!(models, FetchOutcome::Answered(_));
                (window, all_answered)
            }
            ProbeEndpoints::OllamaNative { ps_url } => {
                let ps = fetch(&client, ps_url, None).await;
                let window = match &ps {
                    FetchOutcome::Answered(Some(body)) => {
                        parse_ollama_ps_context_window(body, self.inner.model_id())
                    }
                    _ => None,
                };
                // A model not yet loaded answers with an empty models list;
                // treat as transient so a later request retries once the
                // model is running.
                let answered_with_window_chance = matches!(&ps, FetchOutcome::Answered(Some(_)));
                (window, answered_with_window_chance && window.is_some())
            }
        };

        match window {
            Some(w) => {
                let catalog = self.inner.context_window();
                if w != catalog {
                    tracing::info!(
                        probed = w,
                        catalog,
                        model = self.inner.model_id(),
                        "local server reported its context window; overriding catalog value"
                    );
                }
                let _ = self.window.set(Some(w));
            }
            None => {
                if all_answered {
                    // Definitive: the server is up and simply does not name
                    // a window. Asking again will not change that.
                    tracing::info!(
                        catalog = self.inner.context_window(),
                        model = self.inner.model_id(),
                        "local server answered but named no context window; catalog value stands"
                    );
                    let _ = self.window.set(None);
                } else {
                    // Transient (refused / timeout / 5xx — e.g. the model is
                    // still loading): leave unresolved so a later request
                    // retries. THIS is the session that needs the correction
                    // most — pinning the failure would reproduce the exact
                    // phantom-32K bug this module exists to fix.
                    tracing::debug!(
                        attempt,
                        max = MAX_PROBE_ATTEMPTS,
                        "local context probe could not reach the server; will retry"
                    );
                }
            }
        }
    }
}

/// One best-effort GET. Statuses below 500 are decisive answers (2xx carries
/// a body; 401/404 tell us re-asking is pointless); 5xx and transport
/// failures are transient. The probe must never fail a chat request.
async fn fetch(client: &reqwest::Client, url: &str, api_key: Option<&str>) -> FetchOutcome {
    let mut request = client.get(url);
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }
    match request.send().await {
        Ok(response) => {
            let status = response.status();
            if status.is_success() {
                match response.text().await {
                    Ok(body) => FetchOutcome::Answered(Some(body)),
                    Err(_) => FetchOutcome::Transient,
                }
            } else if status.is_server_error() {
                FetchOutcome::Transient
            } else {
                FetchOutcome::Answered(None)
            }
        }
        Err(_) => FetchOutcome::Transient,
    }
}

#[async_trait]
impl LlmProvider for LocalContextProbe {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.probe_if_unresolved().await;
        self.inner.chat(messages, tools, config).await
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatStream> {
        self.probe_if_unresolved().await;
        self.inner.chat_stream(messages, tools, config).await
    }

    async fn ensure_ready(&self) {
        // Bounded: one probe attempt (a few seconds worst case) while
        // unresolved and attempts remain; immediate once pinned. This is
        // the hook that lets a RESUMED session get the corrected window
        // before its first compaction pass instead of after its first chat.
        self.probe_if_unresolved().await;
    }

    fn context_window(&self) -> u32 {
        self.window
            .get()
            .and_then(|probed| *probed)
            .unwrap_or_else(|| self.inner.context_window())
    }

    fn max_output_tokens(&self) -> u32 {
        self.inner.max_output_tokens()
    }

    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    fn provider_name(&self) -> &str {
        self.inner.provider_name()
    }

    // Full passthrough below (mirrors `ThrottledProvider`): the wrapped
    // OpenAIProvider overrides the metadata methods to split its
    // "label@endpoint" tag — falling back to the trait defaults here would
    // ship the mangled label into usage events and the UI footer.
    fn provider_metadata(&self) -> ProviderMetadata {
        self.inner.provider_metadata()
    }

    fn provider_metadata_for_index(&self, provider_index: Option<usize>) -> ProviderMetadata {
        self.inner.provider_metadata_for_index(provider_index)
    }

    fn export_metrics(&self) -> Option<serde_json::Value> {
        self.inner.export_metrics()
    }

    fn report_late_failure(&self) {
        self.inner.report_late_failure();
    }

    fn report_stream_metrics(&self, output_tokens: u32, stream_duration_us: u64) {
        self.inner
            .report_stream_metrics(output_tokens, stream_duration_us);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TokenUsage;

    struct DummyProvider;

    #[async_trait]
    impl LlmProvider for DummyProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> Result<ChatResponse> {
            Ok(ChatResponse {
                content: Some("ok".into()),
                reasoning_content: None,
                tool_calls: vec![],
                stop_reason: crate::StopReason::EndTurn,
                usage: TokenUsage::default(),
                provider_index: None,
            })
        }

        fn model_id(&self) -> &str {
            "local-default"
        }

        fn provider_name(&self) -> &str {
            "local"
        }
    }

    fn unprobed(base: &str) -> Arc<LocalContextProbe> {
        LocalContextProbe::new(Arc::new(DummyProvider), base, None, None)
    }

    /// llama.cpp default base: `/props` lives at the root, `/models` under
    /// the base. A proxy mounting the API under `/v1/v1` keeps its first
    /// `/v1` (single-suffix strip).
    #[test]
    fn should_derive_probe_urls_from_base() {
        assert_eq!(
            props_url("http://127.0.0.1:8080/v1"),
            "http://127.0.0.1:8080/props"
        );
        assert_eq!(
            models_url("http://127.0.0.1:8080/v1"),
            "http://127.0.0.1:8080/v1/models"
        );
        assert_eq!(
            props_url("http://127.0.0.1:11434/v1/"),
            "http://127.0.0.1:11434/props"
        );
        assert_eq!(props_url("http://gw/v1/v1"), "http://gw/v1/props");
        assert_eq!(
            props_url("http://gpu-box:9000"),
            "http://gpu-box:9000/props"
        );
        assert_eq!(
            ollama_ps_url("http://localhost:11434/v1"),
            "http://localhost:11434/api/ps"
        );
    }

    /// #2135 review P1 regression: the probed window must survive the
    /// STANDARD runtime wrapper (every session wraps the base provider in
    /// RetryProvider) — the trait default would re-read the static catalog
    /// and discard the probe.
    #[test]
    fn should_keep_probed_window_through_retry_wrapper() {
        let probe = unprobed("http://127.0.0.1:8080/v1");
        probe.window.set(Some(262_144)).unwrap();
        let wrapped = crate::RetryProvider::new(probe);
        assert_eq!(wrapped.context_window(), 262_144);
    }

    /// ensure_ready delegates through wrappers and drives the probe: on an
    /// unreachable server it performs a bounded attempt (leaving the window
    /// unresolved for retry), never hanging or panicking.
    #[tokio::test]
    async fn should_drive_probe_through_wrapper_ensure_ready() {
        let probe = unprobed("http://127.0.0.1:9/v1");
        let wrapped = crate::RetryProvider::new(probe.clone());
        wrapped.ensure_ready().await;
        assert!(probe.attempts.load(std::sync::atomic::Ordering::SeqCst) >= 1);
        assert!(probe.window.get().is_none(), "transient must not pin");
    }

    /// Before the probe resolves, the wrapper reports the inner (catalog)
    /// window — `context_window()` must never block. Constructed outside a
    /// runtime here, so no background task races the assertion.
    #[test]
    fn should_fall_back_to_inner_window_before_probe() {
        let inner: Arc<dyn LlmProvider> = Arc::new(DummyProvider);
        let expected = inner.context_window();
        let probe = unprobed("http://127.0.0.1:8080/v1");
        assert_eq!(probe.context_window(), expected);
        assert_eq!(probe.model_id(), "local-default");
        assert_eq!(probe.provider_name(), "local");
    }

    /// A server that ANSWERED both endpoints without naming a window pins
    /// "no window" — the catalog stands and no further probes run.
    #[test]
    fn should_keep_catalog_window_when_server_names_none() {
        let inner: Arc<dyn LlmProvider> = Arc::new(DummyProvider);
        let expected = inner.context_window();
        let probe = unprobed("http://127.0.0.1:8080/v1");
        probe.window.set(None).unwrap();
        assert_eq!(probe.context_window(), expected);
    }

    /// After a successful probe the server-reported window wins.
    #[test]
    fn should_report_probed_window_once_known() {
        let probe = unprobed("http://127.0.0.1:8080/v1");
        probe.window.set(Some(262_144)).unwrap();
        assert_eq!(probe.context_window(), 262_144);
    }

    /// Transient failures do NOT pin: each attempt against an unreachable
    /// server leaves the window unresolved (so a later request retries),
    /// until the attempt cap pins "no window" and requests stop paying for
    /// dead probes.
    #[tokio::test]
    async fn should_retry_transient_failures_then_give_up_at_cap() {
        // Port 9 (discard) refuses connections — every fetch is Transient.
        // Constructed INSIDE the runtime, so one background attempt may
        // also have run; the loop plus the final call always crosses the
        // cap either way.
        let probe = unprobed("http://127.0.0.1:9/v1");
        for _ in 0..MAX_PROBE_ATTEMPTS {
            probe.probe_if_unresolved().await;
        }
        probe.probe_if_unresolved().await; // crosses the cap
        assert_eq!(
            probe.window.get(),
            Some(&None),
            "cap must pin no-window; transient failures alone must not pin a value"
        );
    }
}
