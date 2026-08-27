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
    parse_models_context_window, parse_ollama_ps_context_window, parse_ollama_show_num_ctx,
    parse_props_context_window,
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

/// Overall wall-clock budget for ONE probe attempt, whatever it does
/// inside (#2135 round-4 P2): the per-request timeout applies to each
/// request independently, and the Ollama branch is sequential (ps, show,
/// preload, ps) — without an outer deadline one attempt could block
/// readiness for ~4x the request timeout. Derived from the request
/// timeout, clamped to a human-tolerable range.
fn attempt_deadline_secs(request_timeout_secs: u64) -> u64 {
    (request_timeout_secs * 2).clamp(4, 20)
}

/// Conservative window in force PROVISIONALLY while a cold Ollama model's
/// real allocation is unknown (#2135 rounds 4-6, P1/P2): 1024 — the SAME
/// lower bound this probe's own parsers accept as a plausible window
/// (`sane_context_window`), and below every allocation Ollama's env
/// config will produce in practice (`OLLAMA_CONTEXT_LENGTH=1024` is the
/// smallest configuration the reviewer's audit of envconfig surfaced as
/// realistic; the stock allocation is 4096). Smaller is safer: over-
/// budgeting truncates server-side, under-budgeting only compacts more
/// aggressively. Sub-1024 `num_ctx` values are below the probe's own
/// plausibility threshold everywhere and are out of scope by design; the
/// floor is replaced by the REAL allocation as soon as any attempt reads
/// it from /api/ps — including cheap post-cap refreshes.
const OLLAMA_PROVISIONAL_WINDOW: u32 = 1024;

/// `preload_state` values — see the field doc.
const PRELOAD_IDLE: u32 = 0;
const PRELOAD_RUNNING: u32 = 1;
const PRELOAD_ANSWERED: u32 = 2;
/// Definitive client-error answer (404/401...) from /api/generate: the
/// load will never start this way — no retry, no polling (#2135 round-7
/// P1: mapping these to ANSWERED made every future readiness call poll a
/// full deadline for a load that never existed).
const PRELOAD_FAILED: u32 = 3;

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
    format!("{}/api/ps", ollama_root(base_url))
}

/// Ollama's native model-metadata endpoint, at the server root.
fn ollama_show_url(base_url: &str) -> String {
    format!("{}/api/show", ollama_root(base_url))
}

/// Ollama's native generate endpoint — an empty request is the OFFICIAL
/// preload mechanism (Ollama FAQ): it loads the model and returns, after
/// which `/api/ps` reports the real allocation.
fn ollama_generate_url(base_url: &str) -> String {
    format!("{}/api/generate", ollama_root(base_url))
}

fn ollama_root(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    trimmed.strip_suffix("/v1").unwrap_or(trimmed).to_string()
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
    /// Ollama: `GET /api/ps` (running models' allocated context), with
    /// `POST /api/show` (Modelfile num_ctx) and — when neither answers —
    /// an official empty-request PRELOAD via `POST /api/generate`, after
    /// which `/api/ps` reports the real allocation. Sequential by nature;
    /// one attempt is bounded by a few request timeouts.
    OllamaNative {
        ps_url: String,
        show_url: String,
        generate_url: String,
    },
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
    /// Preload lifecycle (#2135 round-6 P1): 0 = idle (may start), 1 =
    /// running (poll /api/ps), 2 = request answered (load reached the
    /// server; keep polling). A TRANSIENT preload failure (429/503,
    /// transport) resets to idle so a later readiness attempt can retry —
    /// the round-5 boolean latched such failures into "never preload
    /// again, but keep burning the poll deadline anyway".
    preload_state: Arc<AtomicU32>,
    /// Non-zero = a SAFE conservative window in force while the real one is
    /// still unknown (cold Ollama model that could not finish loading
    /// inside the attempt budget). Consulted by `context_window()` after
    /// the pin and before the catalog; replaced by the pin when a later
    /// attempt resolves.
    provisional: AtomicU32,
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
                show_url: ollama_show_url(base_url),
                generate_url: ollama_generate_url(base_url),
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
            preload_state: Arc::new(AtomicU32::new(PRELOAD_IDLE)),
            provisional: AtomicU32::new(0),
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
        let Ok(guard) = self.in_flight.try_lock() else {
            return;
        };
        // PASSIVE (#2135 round-4 P1): background/request-path probes never
        // preload — construction probes every configured provider,
        // including fallback lanes the user may never select, and an eager
        // /api/generate would load all of their models into memory. Only
        // explicit readiness for a route about to be USED may preload.
        self.run_probe_attempt(&guard, false).await;
    }

    /// Spawn the one-and-only detached preload for this probe: an empty
    /// `POST /api/generate`, Ollama's official load trigger. Detached and
    /// LONG-timeout on purpose (#2135 round-5 P1): the load is tied to the
    /// request's lifetime server-side, so neither the attempt deadline nor
    /// the probe's short per-request timeout may own this future. Bounded
    /// by its own generous timeout; fire-and-forget outcome (the /api/ps
    /// polls observe the result).
    fn start_preload(&self, generate_url: &str, model: &str) {
        if self
            .preload_state
            .compare_exchange(
                PRELOAD_IDLE,
                PRELOAD_RUNNING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_err()
        {
            return; // already running or answered
        }
        const PRELOAD_TIMEOUT_SECS: u64 = 900;
        let client = build_http_client(PRELOAD_TIMEOUT_SECS, self.timeouts.1);
        let url = generate_url.to_string();
        let body = serde_json::json!({ "model": model });
        let state = Arc::clone(&self.preload_state);
        let model = model.to_string();
        tracing::info!(model, "preloading cold Ollama model (detached)");
        tokio::spawn(async move {
            match fetch_post_json(&client, &url, &body).await {
                FetchOutcome::Answered(Some(_)) => {
                    state.store(PRELOAD_ANSWERED, Ordering::SeqCst);
                }
                FetchOutcome::Answered(None) => {
                    // Definitive 4xx: the load will never start this way.
                    tracing::warn!(model, "cold-model preload rejected definitively");
                    state.store(PRELOAD_FAILED, Ordering::SeqCst);
                }
                FetchOutcome::Transient => {
                    // 429/503/transport: reset so a later readiness attempt
                    // retries the load instead of polling a load that never
                    // started (#2135 round-6 P1).
                    tracing::debug!(model, "cold-model preload failed transiently; will retry");
                    state.store(PRELOAD_IDLE, Ordering::SeqCst);
                }
            }
        });
    }

    /// Readiness: unlike the request path above, AWAIT an active probe
    /// (lock, don't try_lock) so a caller that is about to make a
    /// window-dependent decision sees the outcome of the in-flight attempt
    /// rather than racing past it (#2135 re-review, P1) — construction
    /// spawns the probe in the background, and returning while it is still
    /// running would hand compaction the stale catalog value one more time.
    /// Still bounded: one attempt's timeouts at most, immediate once pinned.
    async fn await_readiness(&self) {
        if self.window.get().is_some() {
            return;
        }
        match self.in_flight.try_lock() {
            // No attempt active: run exactly one ourselves. Readiness is
            // the one caller allowed to PRELOAD (the route is about to be
            // used; the first chat would trigger the same load).
            Ok(guard) => self.run_probe_attempt(&guard, true).await,
            // An attempt is ACTIVE: await its completion. If it resolved
            // the window, done. If not, that attempt may have been the
            // constructor's PASSIVE probe — which never preloads — so
            // returning here would leave readiness stuck on the floor
            // (#2135 round-5 P1). Run one ACTIVE attempt of our own.
            // Readiness is therefore bounded by at most TWO attempt
            // budgets (the awaited one plus ours), each capped by
            // `attempt_deadline_secs`.
            Err(_) => {
                let guard = self.in_flight.lock().await;
                if self.window.get().is_none() {
                    self.run_probe_attempt(&guard, true).await;
                }
            }
        }
    }

    /// One probe attempt, bounded by [`attempt_deadline_secs`] overall
    /// (#2135 round-4 P2) regardless of how many sequential requests the
    /// branch makes. Caller holds the `in_flight` guard; `allow_preload`
    /// is granted only by readiness (see `probe_if_unresolved`).
    async fn run_probe_attempt(
        &self,
        _guard: &tokio::sync::MutexGuard<'_, ()>,
        allow_preload: bool,
    ) {
        if self.window.get().is_some() {
            return;
        }
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        if attempt > MAX_PROBE_ATTEMPTS && self.provisional.load(Ordering::SeqCst) == 0 {
            // Spent: stop paying failed GETs on every request. Pinning "no
            // window" keeps the catalog value without further probing.
            //
            // EXCEPT while a provisional floor is in force (#2135 round-5
            // P2): pinning would freeze the floor forever even after the
            // real chat loads the model. Post-cap attempts continue in the
            // Ollama branch as ONE cheap /api/ps read each (the floor
            // being set skips the show/preload path), so the real
            // allocation is adopted whenever the model comes up.
            let _ = self.window.set(None);
            return;
        }
        let budget = std::time::Duration::from_secs(attempt_deadline_secs(self.timeouts.0));
        if tokio::time::timeout(budget, self.attempt_body(attempt, allow_preload))
            .await
            .is_err()
        {
            tracing::debug!(
                attempt,
                budget_secs = budget.as_secs(),
                "context probe attempt exceeded its overall deadline; will retry"
            );
        }
    }

    async fn attempt_body(&self, attempt: u32, allow_preload: bool) {
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
            ProbeEndpoints::OllamaNative {
                ps_url,
                show_url,
                generate_url,
            } => {
                let model = self.inner.model_id();
                let ps = fetch(&client, ps_url, None).await;
                let mut window = match &ps {
                    FetchOutcome::Answered(Some(body)) => {
                        parse_ollama_ps_context_window(body, model)
                    }
                    _ => None,
                };
                // COLD model (#2135 re-review, P2): before the model loads,
                // /api/ps lists nothing and the FIRST turn would size its
                // prompt from the catalog. /api/show answers from the
                // registry, and a Modelfile num_ctx is runtime
                // configuration — safe to pin now.
                let server_up = matches!(&ps, FetchOutcome::Answered(Some(_)));
                if window.is_none() && server_up {
                    // Once the floor is in force, later attempts skip the
                    // show/preload path entirely: each is ONE cheap /api/ps
                    // read (this is what keeps post-cap refreshes nearly
                    // free — see the attempt-cap comment).
                    if self.provisional.load(Ordering::SeqCst) == 0 {
                        let body = serde_json::json!({ "model": model });
                        if let FetchOutcome::Answered(Some(show)) =
                            fetch_post_json(&client, show_url, &body).await
                        {
                            window = parse_ollama_show_num_ctx(&show);
                            // No Modelfile num_ctx: the effective window is
                            // decided at LOAD time. Put the conservative
                            // floor in force IMMEDIATELY — before any
                            // preload — so whatever happens next (passive
                            // probe stops here; a slow load outlives the
                            // attempt deadline), the first turn compacts
                            // against a window the server is very unlikely
                            // to undercut, instead of catalog maxima
                            // (#2135 round-4/5 P1).
                            if window.is_none() {
                                self.provisional
                                    .store(OLLAMA_PROVISIONAL_WINDOW, Ordering::SeqCst);
                                tracing::info!(
                                    model,
                                    provisional = OLLAMA_PROVISIONAL_WINDOW,
                                    "cold Ollama model: conservative provisional window in \
                                     force until the load reports its allocation"
                                );
                            }
                        }
                    }
                    // PRELOAD — Ollama's official empty /api/generate
                    // request — ONLY when readiness granted it (#2135
                    // round-4 P1: passive probes must never load models on
                    // lanes the user did not select). The request runs as a
                    // DETACHED task with a long-timeout client (#2135
                    // round-5 P1): Ollama ties scheduling of the load to
                    // the live request context, so the attempt deadline
                    // cancelling the request could abandon the load and
                    // loop the model cold forever. The deadline cancels
                    // only the POLLING below; the load itself runs on.
                    if window.is_none() && allow_preload {
                        self.start_preload(generate_url, model);
                        // Poll only while a load is actually in flight or
                        // reached the server; a transient preload failure
                        // resets to idle and a definitive one parks at
                        // FAILED — either way this loop stops instead of
                        // burning the attempt deadline (#2135 rounds 6-7).
                        loop {
                            let state = self.preload_state.load(Ordering::SeqCst);
                            if state != PRELOAD_RUNNING && state != PRELOAD_ANSWERED {
                                break;
                            }
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                            if let FetchOutcome::Answered(Some(ps_after)) =
                                fetch(&client, ps_url, None).await
                            {
                                window = parse_ollama_ps_context_window(&ps_after, model);
                                // Load COMPLETE (generate answered) and the
                                // server's process view still names no
                                // usable window — an old Ollama without
                                // context_length, or an entry we cannot
                                // match. That view will not improve: pin
                                // "no window" so later turns stop paying a
                                // poll deadline; the conservative floor
                                // stays in force permanently (#2135
                                // round-7 P1).
                                if window.is_none()
                                    && self.preload_state.load(Ordering::SeqCst) == PRELOAD_ANSWERED
                                {
                                    tracing::info!(
                                        model,
                                        "loaded model reports no usable allocation; \
                                         keeping the conservative floor permanently"
                                    );
                                    let _ = self.window.set(None);
                                    break;
                                }
                            }
                            if window.is_some() {
                                break;
                            }
                        }
                    }
                }
                // Still unknown (server unreachable, or the load is still
                // in progress): stay unresolved so a later request retries
                // once the model is running.
                let resolved = window.is_some();
                (window, resolved)
            }
        };

        match window {
            Some(w) => {
                self.provisional.store(0, Ordering::SeqCst);
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
                } else if self.window.get().is_none() {
                    // Transient (refused / timeout / 5xx — e.g. the model is
                    // still loading): leave unresolved so a later request
                    // retries. THIS is the session that needs the correction
                    // most — pinning the failure would reproduce the exact
                    // phantom-32K bug this module exists to fix. (Already-
                    // pinned outcomes — e.g. the loaded-but-unreadable
                    // Ollama case pinned inside the poll loop — skip this.)
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

/// Whether a failed HTTP status is worth retrying later. 5xx (server not
/// ready), 408 (request timeout), 425 (too early), and 429 (rate limited)
/// are TRANSIENT — pinning "no window" on a rate-limited probe would
/// permanently keep the catalog value with no recovery (#2135 round-3
/// P2). Other client errors (401, 403, 404) are decisive: re-asking will
/// not change them.
fn status_is_transient(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || matches!(status.as_u16(), 408 | 425 | 429)
}

/// One best-effort POST with a JSON body (Ollama /api/show, /api/generate).
/// Same outcome semantics as [`fetch`].
async fn fetch_post_json(
    client: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
) -> FetchOutcome {
    match client.post(url).json(body).send().await {
        Ok(response) => {
            let status = response.status();
            if status.is_success() {
                match response.text().await {
                    Ok(text) => FetchOutcome::Answered(Some(text)),
                    Err(_) => FetchOutcome::Transient,
                }
            } else if status_is_transient(status) {
                FetchOutcome::Transient
            } else {
                FetchOutcome::Answered(None)
            }
        }
        Err(_) => FetchOutcome::Transient,
    }
}

/// One best-effort GET. 2xx carries a body; retryable statuses (see
/// [`status_is_transient`]) and transport failures are transient; other
/// client errors (401/404) are decisive answers. The probe must never fail
/// a chat request.
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
            } else if status_is_transient(status) {
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
        // Bounded: waits for an ACTIVE probe to finish (or runs one
        // attempt itself), a few seconds worst case; immediate once
        // pinned. This is the hook that lets a RESUMED session get the
        // corrected window before its first compaction pass instead of
        // after its first chat.
        self.await_readiness().await;
    }

    fn context_window(&self) -> u32 {
        if let Some(Some(window)) = self.window.get() {
            return *window;
        }
        // A provisional floor (cold Ollama, load still in progress) beats
        // the catalog: it is conservative in the only direction that
        // cannot overflow the server.
        let provisional = self.provisional.load(Ordering::SeqCst);
        if provisional != 0 {
            return provisional;
        }
        self.inner.context_window()
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

    /// Minimal Ollama-shaped HTTP stub: records request paths, serves
    /// /api/ps (allocation only once "loaded"), /api/show (no Modelfile
    /// num_ctx), and /api/generate (sets "loaded" after `generate_delay`).
    async fn spawn_ollama_stub(
        generate_delay: std::time::Duration,
    ) -> (
        String,
        Arc<std::sync::Mutex<Vec<String>>>,
        Arc<std::sync::atomic::AtomicBool>,
    ) {
        spawn_ollama_stub_failing(generate_delay, 0).await
    }

    /// Like [`spawn_ollama_stub`], with the first `generate_failures`
    /// /api/generate requests answered 503 (transient) without loading.
    async fn spawn_ollama_stub_failing(
        generate_delay: std::time::Duration,
        generate_failures: u32,
    ) -> (
        String,
        Arc<std::sync::Mutex<Vec<String>>>,
        Arc<std::sync::atomic::AtomicBool>,
    ) {
        spawn_ollama_stub_configured(generate_delay, generate_failures, true, false).await
    }

    /// Full-fidelity stub: `ps_with_context` = whether /api/ps includes
    /// context_length once loaded (old Ollama does not);
    /// `generate_definitive_fail` = answer every /api/generate with 404.
    async fn spawn_ollama_stub_configured(
        generate_delay: std::time::Duration,
        generate_failures: u32,
        ps_with_context: bool,
        generate_definitive_fail: bool,
    ) -> (
        String,
        Arc<std::sync::Mutex<Vec<String>>>,
        Arc<std::sync::atomic::AtomicBool>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let hits: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let loaded = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let loaded_out = loaded.clone();
        let failures_left = Arc::new(std::sync::atomic::AtomicU32::new(generate_failures));
        let hits_srv = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let hits = hits_srv.clone();
                let loaded = loaded.clone();
                let failures_left = failures_left.clone();
                let delay = generate_delay;
                let ps_with_context = ps_with_context;
                let generate_definitive_fail = generate_definitive_fail;
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                    hits.lock().unwrap().push(path.clone());
                    let body = match path.as_str() {
                        "/api/ps" => {
                            if loaded.load(std::sync::atomic::Ordering::SeqCst) {
                                if ps_with_context {
                                    r#"{"models":[{"name":"local-default","context_length":32768}]}"#
                                        .to_string()
                                } else {
                                    // Old Ollama: process entry without the field.
                                    r#"{"models":[{"name":"local-default"}]}"#.to_string()
                                }
                            } else {
                                r#"{"models":[]}"#.to_string()
                            }
                        }
                        "/api/show" => r#"{"parameters":"stop "x""}"#.to_string(),
                        "/api/generate" => {
                            if generate_definitive_fail {
                                let response = "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                                let _ = stream.write_all(response.as_bytes()).await;
                                return;
                            }
                            if failures_left
                                .fetch_update(
                                    std::sync::atomic::Ordering::SeqCst,
                                    std::sync::atomic::Ordering::SeqCst,
                                    |n| n.checked_sub(1),
                                )
                                .is_ok()
                            {
                                let response = "HTTP/1.1 503 Service Unavailable\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                                let _ = stream.write_all(response.as_bytes()).await;
                                return;
                            }
                            tokio::time::sleep(delay).await;
                            loaded.store(true, std::sync::atomic::Ordering::SeqCst);
                            "{}".to_string()
                        }
                        _ => "{}".to_string(),
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        (base, hits, loaded_out)
    }

    /// #2135 round-4 P1 regression: construction and the request path are
    /// PASSIVE — they must never hit /api/generate (an eager preload would
    /// load every configured Ollama lane's model into memory) — but they DO
    /// put the conservative provisional floor in force for a cold model.
    #[tokio::test]
    async fn should_not_preload_from_construction_or_request_path() {
        let (base, hits, _loaded) = spawn_ollama_stub(std::time::Duration::ZERO).await;
        let probe = LocalContextProbe::new_ollama(Arc::new(DummyProvider), &base, Some((2, 1)));
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        probe.probe_if_unresolved().await;
        let paths = hits.lock().unwrap().clone();
        assert!(
            !paths.iter().any(|p| p == "/api/generate"),
            "passive probes must never preload; hits: {paths:?}"
        );
        assert!(paths.iter().any(|p| p == "/api/ps"), "hits: {paths:?}");
        assert!(probe.window.get().is_none(), "cold model stays unresolved");
        assert_eq!(
            probe.context_window(),
            OLLAMA_PROVISIONAL_WINDOW,
            "conservative floor must be in force"
        );
    }

    /// #2135 round-4 P1 regression (delayed preload): readiness preloads
    /// the SELECTED route, polls /api/ps, and adopts the real allocation
    /// once the (slow) load completes within the attempt budget.
    #[tokio::test]
    async fn should_preload_and_adopt_allocation_on_readiness() {
        let (base, hits, _loaded) = spawn_ollama_stub(std::time::Duration::from_millis(300)).await;
        let probe = LocalContextProbe::new_ollama(Arc::new(DummyProvider), &base, Some((2, 1)));
        probe.ensure_ready().await;
        assert_eq!(probe.window.get(), Some(&Some(32_768)));
        assert_eq!(probe.context_window(), 32_768);
        assert_eq!(
            probe.provisional.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "provisional floor is cleared once the real allocation pins"
        );
        let generates = hits
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.as_str() == "/api/generate")
            .count();
        assert_eq!(generates, 1, "readiness preloads exactly once");
    }

    /// #2135 round-4 P1+P2 regression: a load that outlives the attempt
    /// deadline leaves readiness bounded (single overall deadline, not a
    /// per-request multiple) with the provisional floor in force.
    #[tokio::test]
    async fn should_bound_readiness_and_keep_floor_when_load_outlives_deadline() {
        let (base, _hits, _loaded) = spawn_ollama_stub(std::time::Duration::from_secs(60)).await;
        let probe = LocalContextProbe::new_ollama(Arc::new(DummyProvider), &base, Some((1, 1)));
        let start = std::time::Instant::now();
        probe.ensure_ready().await;
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(attempt_deadline_secs(1) + 3),
            "readiness must respect the overall attempt deadline; took {elapsed:?}"
        );
        assert!(probe.window.get().is_none(), "still unresolved for retry");
        assert_eq!(
            probe.context_window(),
            OLLAMA_PROVISIONAL_WINDOW,
            "floor stays in force while the load continues server-side"
        );
    }

    /// #2135 round-5 P1 regression: when readiness had to WAIT for an
    /// in-flight PASSIVE attempt that left the window unresolved, it must
    /// run one ACTIVE attempt of its own — returning at the floor without
    /// ever preloading was the bug.
    #[tokio::test]
    async fn should_run_active_attempt_when_awaited_passive_left_unresolved() {
        let (base, hits, _loaded) = spawn_ollama_stub(std::time::Duration::from_millis(200)).await;
        let probe = LocalContextProbe::new_ollama(Arc::new(DummyProvider), &base, Some((2, 1)));
        // Simulate the constructor's passive probe holding the lock and
        // finishing WITHOUT resolving (it never preloads).
        let guard = probe.in_flight.lock().await;
        let waiter = {
            let probe = probe.clone();
            tokio::spawn(async move {
                probe.ensure_ready().await;
                probe.context_window()
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        drop(guard); // passive attempt "completed", window still unresolved
        assert_eq!(
            waiter.await.unwrap(),
            32_768,
            "readiness must preload itself"
        );
        assert!(
            hits.lock().unwrap().iter().any(|p| p == "/api/generate"),
            "the active readiness attempt must have preloaded"
        );
    }

    /// #2135 round-5 P1+P2 regression: the detached preload survives the
    /// attempt deadline (the deadline cancels only the polling), and the
    /// real allocation is adopted EVEN AFTER the retry cap — a provisional
    /// floor must never be frozen by the cap.
    #[tokio::test]
    async fn should_adopt_allocation_after_deadline_and_after_retry_cap() {
        // Load takes 6s; timeouts (1,1) give a 4s attempt deadline.
        let (base, _hits, loaded) = spawn_ollama_stub(std::time::Duration::from_secs(6)).await;
        let probe = LocalContextProbe::new_ollama(Arc::new(DummyProvider), &base, Some((1, 1)));
        probe.ensure_ready().await;
        assert!(probe.window.get().is_none(), "load outlives the deadline");
        assert_eq!(probe.context_window(), OLLAMA_PROVISIONAL_WINDOW);

        // Burn past the retry cap while the model is still loading: with a
        // floor in force the cap must NOT pin None.
        for _ in 0..(MAX_PROBE_ATTEMPTS + 2) {
            probe.probe_if_unresolved().await;
        }
        assert!(
            probe.window.get().is_none(),
            "cap must not pin over a floor"
        );

        // The DETACHED preload finishes server-side (it was not cancelled
        // with the attempt); the next cheap refresh adopts the allocation.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !loaded.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(std::time::Instant::now() < deadline, "stub never loaded");
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        probe.probe_if_unresolved().await;
        assert_eq!(probe.window.get(), Some(&Some(32_768)));
        assert_eq!(probe.context_window(), 32_768);
        assert_eq!(
            probe.provisional.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "floor cleared once the real allocation pins"
        );
    }

    /// #2135 round-6 P1 regression: a TRANSIENT preload failure (503)
    /// resets the preload state — the poll loop stops early instead of
    /// burning the deadline, and the NEXT readiness attempt retries the
    /// load and adopts the allocation. The round-5 boolean latched the
    /// failure forever.
    #[tokio::test]
    async fn should_retry_preload_after_transient_failure() {
        let (base, hits, _loaded) =
            spawn_ollama_stub_failing(std::time::Duration::from_millis(100), 1).await;
        let probe = LocalContextProbe::new_ollama(Arc::new(DummyProvider), &base, Some((2, 1)));
        // Attempt 1: preload 503s; state resets; readiness returns with the
        // floor, well before the deadline (poll loop must stop early).
        let start = std::time::Instant::now();
        probe.ensure_ready().await;
        assert!(probe.window.get().is_none());
        assert_eq!(probe.context_window(), OLLAMA_PROVISIONAL_WINDOW);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(attempt_deadline_secs(2)),
            "failed preload must not burn the full deadline"
        );
        // Attempt 2: preload retries and the allocation is adopted.
        probe.ensure_ready().await;
        assert_eq!(probe.window.get(), Some(&Some(32_768)));
        let generates = hits
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.as_str() == "/api/generate")
            .count();
        assert_eq!(generates, 2, "one failed + one successful preload");
    }

    /// #2135 round-7 P1 regression (old Ollama): once the load completes
    /// but /api/ps names no usable allocation, the outcome pins as "no
    /// window" — the floor stays permanently and LATER readiness calls
    /// return immediately instead of polling a full deadline every turn.
    #[tokio::test]
    async fn should_pin_and_stop_polling_when_loaded_model_reports_no_allocation() {
        let (base, hits, _loaded) = spawn_ollama_stub_configured(
            std::time::Duration::from_millis(100),
            0,
            false, // old Ollama: no context_length on /api/ps
            false,
        )
        .await;
        let probe = LocalContextProbe::new_ollama(Arc::new(DummyProvider), &base, Some((2, 1)));
        probe.ensure_ready().await;
        assert_eq!(probe.window.get(), Some(&None), "definitively pinned");
        assert_eq!(probe.context_window(), OLLAMA_PROVISIONAL_WINDOW);
        let start = std::time::Instant::now();
        probe.ensure_ready().await;
        assert!(
            start.elapsed() < std::time::Duration::from_millis(500),
            "pinned outcome must return immediately"
        );
        let generates = hits
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.as_str() == "/api/generate")
            .count();
        assert_eq!(generates, 1);
    }

    /// #2135 round-7 P1 regression (definitive preload failure): a 404 on
    /// /api/generate parks the preload at FAILED — no polling, no retry,
    /// readiness returns fast on every call with the floor in force.
    #[tokio::test]
    async fn should_not_poll_after_definitive_preload_failure() {
        let (base, hits, _loaded) = spawn_ollama_stub_configured(
            std::time::Duration::ZERO,
            0,
            true,
            true, // /api/generate always 404
        )
        .await;
        let probe = LocalContextProbe::new_ollama(Arc::new(DummyProvider), &base, Some((2, 1)));
        for _ in 0..2 {
            let start = std::time::Instant::now();
            probe.ensure_ready().await;
            assert!(
                start.elapsed() < std::time::Duration::from_secs(2),
                "failed preload must not burn poll deadlines"
            );
        }
        assert_eq!(probe.context_window(), OLLAMA_PROVISIONAL_WINDOW);
        let generates = hits
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.as_str() == "/api/generate")
            .count();
        assert_eq!(
            generates, 1,
            "definitive failure must not retry the preload"
        );
    }

    /// #2135 round-3 P2: retryable client statuses must classify as
    /// transient — the reviewer's scenario was /props 404 (decisive) plus
    /// /models 429 (rate limited): with 429 marked decisive, all_answered
    /// pinned "no window" permanently and later recovery was impossible.
    #[test]
    fn should_treat_retryable_statuses_as_transient() {
        use reqwest::StatusCode;
        for code in [408u16, 425, 429, 500, 502, 503] {
            assert!(
                status_is_transient(StatusCode::from_u16(code).unwrap()),
                "{code} must be transient"
            );
        }
        for code in [400u16, 401, 403, 404, 410] {
            assert!(
                !status_is_transient(StatusCode::from_u16(code).unwrap()),
                "{code} is decisive"
            );
        }
    }

    /// #2135 re-review P1 regression (delayed server): ensure_ready must
    /// AWAIT an active probe, not race past it — a background attempt still
    /// in flight used to make readiness return with the window unknown, and
    /// compaction read the catalog value anyway.
    #[tokio::test]
    async fn should_await_active_probe_completion_in_ensure_ready() {
        let probe = unprobed("http://127.0.0.1:9/v1");
        // Simulate a slow background probe: hold the in-flight guard.
        let guard = probe.in_flight.lock().await;
        let waiter = {
            let probe = probe.clone();
            tokio::spawn(async move {
                probe.ensure_ready().await;
                probe.context_window()
            })
        };
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !waiter.is_finished(),
            "readiness must block while a probe attempt is active"
        );
        // The "background probe" resolves, then releases the guard.
        probe.window.set(Some(262_144)).unwrap();
        drop(guard);
        assert_eq!(waiter.await.unwrap(), 262_144);
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
