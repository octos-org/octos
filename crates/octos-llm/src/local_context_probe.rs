//! Probe a local server for its actual context window.
//!
//! The catalog row for `local/local-default` carries a deliberately modest
//! `context_window` (32K) because at registration time nothing knows what the
//! operator launched. But the running server DOES know: llama.cpp's `/props`
//! reports the `-c` value it was started with, and every OpenAI-compatible
//! engine exposes some spelling of the window on `GET /v1/models` (see
//! [`crate::local_discovery`]). Budgeting a 256K server as 32K is not a safe
//! under-estimate — the compaction loop shreds the working set to fit the
//! phantom limit, and long tasks degrade into re-read thrash (observed: a
//! 1,182-line source file read 66 times in one session while the live context
//! sat at ~19K of an actual 256K).
//!
//! [`LocalContextProbe`] wraps a local provider and asks the server once, on
//! the first request, in the async context that request already provides. The
//! probe is best-effort with a short timeout: a server that answers neither
//! endpoint costs one round of two quick failed GETs and the catalog value
//! stands. `context_window()` is sync and never blocks — before the first
//! request completes it reports the inner (catalog) value, after that the
//! probed one. The first request is the system prompt plus one user turn, far
//! below any plausible budget, so correcting the window from the second
//! request onward is safe.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use octos_core::Message;
use tokio::sync::OnceCell;

use crate::config::ChatConfig;
use crate::local_discovery::{parse_models_context_window, parse_props_context_window};
use crate::provider::{LlmProvider, build_http_client};
use crate::types::{ChatResponse, ChatStream, ToolSpec};

/// Probe timeouts: a local server answers these endpoints in milliseconds;
/// anything slower is a server that is not going to answer at all, and the
/// user is watching this latency on their first message.
const PROBE_TIMEOUT_SECS: u64 = 3;
const PROBE_CONNECT_TIMEOUT_SECS: u64 = 2;

/// llama.cpp serves `/props` at the server root, not under `/v1`.
fn props_url(base_url: &str) -> String {
    let root = base_url
        .trim_end_matches('/')
        .trim_end_matches("/v1")
        .trim_end_matches('/');
    format!("{root}/props")
}

/// The OpenAI list-models endpoint, relative to the configured base.
fn models_url(base_url: &str) -> String {
    format!("{}/models", base_url.trim_end_matches('/'))
}

/// Wraps a local-family provider; overrides `context_window()` with the
/// server-reported value once the first request has triggered the probe.
pub struct LocalContextProbe {
    inner: Arc<dyn LlmProvider>,
    props_url: String,
    models_url: String,
    probed: OnceCell<Option<u32>>,
}

impl LocalContextProbe {
    pub fn new(inner: Arc<dyn LlmProvider>, base_url: &str) -> Self {
        Self {
            inner,
            props_url: props_url(base_url),
            models_url: models_url(base_url),
            probed: OnceCell::new(),
        }
    }

    /// Run the probe exactly once; concurrent first requests coalesce on the
    /// same `OnceCell` initialization.
    async fn ensure_probed(&self) {
        self.probed
            .get_or_init(|| async {
                let client = build_http_client(PROBE_TIMEOUT_SECS, PROBE_CONNECT_TIMEOUT_SECS);
                // `/props` first: it reports the window the server was
                // LAUNCHED with, which caps whatever the model metadata says.
                let from_props = fetch(&client, &self.props_url)
                    .await
                    .as_deref()
                    .and_then(parse_props_context_window);
                let window = match from_props {
                    Some(w) => Some(w),
                    None => fetch(&client, &self.models_url)
                        .await
                        .as_deref()
                        .and_then(parse_models_context_window),
                };
                match window {
                    Some(w) => {
                        let catalog = self.inner.context_window();
                        if w != catalog {
                            tracing::info!(
                                probed = w,
                                catalog,
                                "local server reported its context window; overriding catalog value"
                            );
                        }
                    }
                    None => tracing::debug!(
                        props_url = %self.props_url,
                        models_url = %self.models_url,
                        "local server did not report a context window; keeping catalog value"
                    ),
                }
                window
            })
            .await;
    }
}

/// One best-effort GET; any failure (refused, timeout, non-2xx, body read
/// error) collapses to `None` — the probe must never fail a chat request.
async fn fetch(client: &reqwest::Client, url: &str) -> Option<String> {
    let response = client.get(url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.text().await.ok()
}

#[async_trait]
impl LlmProvider for LocalContextProbe {
    async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        self.ensure_probed().await;
        self.inner.chat(messages, tools, config).await
    }

    async fn chat_stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatStream> {
        self.ensure_probed().await;
        self.inner.chat_stream(messages, tools, config).await
    }

    fn context_window(&self) -> u32 {
        self.probed
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

    fn report_late_failure(&self) {
        self.inner.report_late_failure();
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

    /// llama.cpp default base: `/props` lives at the root, `/models` under
    /// the base. Bases without a `/v1` suffix keep their root.
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
        assert_eq!(
            props_url("http://gpu-box:9000"),
            "http://gpu-box:9000/props"
        );
    }

    /// Before any request has run the probe, the wrapper reports the inner
    /// (catalog) window — `context_window()` must never block.
    #[test]
    fn should_fall_back_to_inner_window_before_probe() {
        let inner: Arc<dyn LlmProvider> = Arc::new(DummyProvider);
        let expected = inner.context_window();
        let probe = LocalContextProbe::new(inner, "http://127.0.0.1:8080/v1");
        assert_eq!(probe.context_window(), expected);
        assert_eq!(probe.model_id(), "local-default");
        assert_eq!(probe.provider_name(), "local");
    }

    /// A probe that found nothing (server answered neither endpoint) pins
    /// `None` and the catalog value continues to stand — permanently, not
    /// retried per request.
    #[tokio::test]
    async fn should_keep_catalog_window_when_probe_found_nothing() {
        let inner: Arc<dyn LlmProvider> = Arc::new(DummyProvider);
        let expected = inner.context_window();
        let probe = LocalContextProbe::new(inner, "http://127.0.0.1:8080/v1");
        probe.probed.set(None).unwrap();
        assert_eq!(probe.context_window(), expected);
    }

    /// After a successful probe the server-reported window wins.
    #[tokio::test]
    async fn should_report_probed_window_once_known() {
        let inner: Arc<dyn LlmProvider> = Arc::new(DummyProvider);
        let probe = LocalContextProbe::new(inner, "http://127.0.0.1:8080/v1");
        probe.probed.set(Some(262_144)).unwrap();
        assert_eq!(probe.context_window(), 262_144);
    }
}
