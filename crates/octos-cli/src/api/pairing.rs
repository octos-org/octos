//! One-time pairing: open the web client from a printed link.
//!
//! WEB-PAIRING-CONTRACT-5100 (`## Server`). `octos serve` mints exactly ONE
//! pairing code per process start and prints it on stdout. A loopback client
//! exchanges that code, once, for the server's API token — the same bearer
//! token the rest of the surface already requires. Pairing is an additional
//! way to OBTAIN that token, never a second way to bypass it: every
//! authenticated route keeps its middleware untouched.
//!
//! ## Security model
//!
//! - **Loopback only.** Both endpoints answer `404` — not `403` — to any
//!   non-loopback peer, and to any request carrying reverse-proxy headers
//!   (the same laundering defence [`super::solo_auth`] uses: a Caddy-fronted
//!   daemon sees every external request arrive over loopback). 404 keeps the
//!   endpoints from confirming the server exists to the wider network.
//! - **CSPRNG.** The code is 8 characters drawn from the OS CSPRNG
//!   ([`rand::rngs::OsRng`]) over Crockford base32 (no `I`/`L`/`O`/`U`).
//!   32 divides 256, so the rejection-free `byte & 31` mapping is unbiased.
//! - **Constant time.** The submitted code is compared with
//!   [`subtle::ConstantTimeEq`] after normalisation, never with `==`.
//! - **One guarded state.** The attempt budget AND the single-use burn live
//!   behind ONE mutex, taken once per claim, so two concurrent claims of the
//!   correct code cannot both be handed the token.
//! - **Never logged.** The code, the token and the claim body are never
//!   passed to `tracing` at any level. The code reaches stdout exactly once,
//!   at startup, via `serve_console`.

use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{ConnectInfo, FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

use super::AppState;

/// Length of a printed pairing code.
pub const PAIR_CODE_LEN: usize = 8;

/// Crockford base32 — digits plus consonant-ish letters with `I`, `L`, `O`
/// and `U` removed so a code read off a terminal cannot be mistyped into a
/// different valid code. Exactly 32 symbols, so `byte & 31` is unbiased.
pub const PAIR_CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A pairing code is valid for five minutes from mint.
pub const PAIR_CODE_TTL: Duration = Duration::from_secs(300);

/// At most this many FAILED claims per process; the next failure burns the
/// code and every later claim answers `pair_code_locked`.
pub const PAIR_MAX_FAILED_CLAIMS: u32 = 10;

/// The four error kinds the contract allows. The wire form carries the kind
/// and nothing else — no hint about which rule fired beyond the kind itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairError {
    /// Wrong code, or a code already burned by a successful claim.
    Unknown,
    /// Past the five-minute window.
    Expired,
    /// The attempt budget is spent; the code is burned.
    Locked,
    /// The body is not `{ "code": "<8 chars of the alphabet>" }`. NOT a
    /// guess: it never touches the attempt budget.
    Invalid,
}

impl PairError {
    /// The `error.kind` string.
    pub fn kind(self) -> &'static str {
        match self {
            PairError::Unknown => "pair_code_unknown",
            PairError::Expired => "pair_code_expired",
            PairError::Locked => "pair_code_locked",
            PairError::Invalid => "pair_code_invalid",
        }
    }
}

/// Terminal state of the single per-process code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodeStatus {
    /// Claimable (subject to expiry).
    Live,
    /// Burned by a successful claim — single use.
    Used,
    /// Burned by the attempt budget.
    Locked,
}

/// Everything a claim mutates, behind ONE lock: the burn flag and the
/// attempt budget. Keeping them in the same guarded state is what makes a
/// race between two concurrent claims impossible to win twice.
#[derive(Debug)]
struct PairingInner {
    code: [u8; PAIR_CODE_LEN],
    status: CodeStatus,
    failures: u32,
    /// With a failure window: when the current window's first failure was.
    window_start: Option<Instant>,
}

/// The process-wide pairing state: one code, one token, one origin.
#[derive(Debug)]
pub struct PairingState {
    inner: Mutex<PairingInner>,
    minted_at: Instant,
    ttl: Duration,
    /// The API token handed out on a successful claim. Empty when the server
    /// runs without a bearer token, in which case `pairing_required` is false
    /// and a client has nothing to obtain.
    token: String,
    /// `http://127.0.0.1:<port>` for the bound listener.
    server_origin: String,
    /// `None`: the attempt budget burns the code for good (the printed-code
    /// contract). `Some(w)`: a rate limit instead: the budget refills `w`
    /// after its first failure, so guessing cannot deny a host-shown code.
    failure_window: Option<Duration>,
}

impl PairingState {
    /// Mint the process's single pairing code from the OS CSPRNG.
    pub fn mint(server_origin: impl Into<String>, token: Option<String>) -> Self {
        Self::mint_with_ttl(server_origin, token, PAIR_CODE_TTL)
    }

    /// [`Self::mint`] with an explicit TTL. Tests use a zero TTL to reach the
    /// expiry branch without sleeping.
    pub fn mint_with_ttl(
        server_origin: impl Into<String>,
        token: Option<String>,
        ttl: Duration,
    ) -> Self {
        Self {
            inner: Mutex::new(PairingInner {
                code: mint_code(),
                status: CodeStatus::Live,
                failures: 0,
                window_start: None,
            }),
            minted_at: Instant::now(),
            ttl,
            token: token.unwrap_or_default(),
            server_origin: server_origin.into(),
            failure_window: None,
        }
    }

    /// A code whose attempt budget is a rate limit ([`PAIR_MAX_FAILED_CLAIMS`]
    /// failures per `window`) rather than a permanent lock. For a code a host
    /// shows on demand (`octos serve --host-managed`).
    pub fn mint_rate_limited(
        server_origin: impl Into<String>,
        token: Option<String>,
        ttl: Duration,
        window: Duration,
    ) -> Self {
        Self {
            failure_window: Some(window),
            ..Self::mint_with_ttl(server_origin, token, ttl)
        }
    }

    /// The printed code. Called exactly once, by `octos serve`, to write the
    /// startup line to stdout — never handed to `tracing`.
    pub fn printed_code(&self) -> String {
        let inner = self.lock();
        String::from_utf8_lossy(&inner.code).into_owned()
    }

    /// `http://127.0.0.1:<port>`.
    pub fn server_origin(&self) -> &str {
        &self.server_origin
    }

    /// Whether a client needs a token at all. False on a server running
    /// without a bearer token — the client can connect unauthenticated.
    pub fn pairing_required(&self) -> bool {
        !self.token.is_empty()
    }

    /// Exchange `submitted` for the API token.
    ///
    /// One lock, one decision: expiry, the constant-time comparison, the
    /// single-use burn and the attempt budget are all resolved inside the
    /// same critical section.
    pub fn claim(&self, submitted: &str) -> Result<String, PairError> {
        self.claim_at(submitted, Instant::now())
    }

    fn claim_at(&self, submitted: &str, now: Instant) -> Result<String, PairError> {
        // Shape check FIRST and outside the budget: a malformed body is not a
        // guess, so it must not consume an attempt.
        let submitted = normalize_code(submitted).ok_or(PairError::Invalid)?;

        let mut inner = self.lock();
        if let Some(window) = self.failure_window {
            let elapsed = inner
                .window_start
                .is_some_and(|start| now.saturating_duration_since(start) >= window);
            if elapsed {
                inner.failures = 0;
                inner.window_start = None;
                if inner.status == CodeStatus::Locked {
                    inner.status = CodeStatus::Live;
                }
            }
        }
        match inner.status {
            CodeStatus::Used => return Err(PairError::Unknown),
            CodeStatus::Locked => return Err(PairError::Locked),
            CodeStatus::Live => {}
        }
        if now.saturating_duration_since(self.minted_at) >= self.ttl {
            return Err(PairError::Expired);
        }

        if bool::from(submitted.ct_eq(&inner.code)) {
            // Burn before releasing the lock: a concurrent claim that is
            // already blocked on this mutex sees `Used` and gets
            // `pair_code_unknown`, never a second copy of the token.
            inner.status = CodeStatus::Used;
            return Ok(self.token.clone());
        }

        if inner.failures == 0 {
            inner.window_start = Some(now);
        }
        inner.failures = inner.failures.saturating_add(1);
        if inner.failures >= PAIR_MAX_FAILED_CLAIMS {
            // The budget is spent: burn the code in the SAME critical section
            // that counted the failure, and answer `locked` from here on.
            inner.status = CodeStatus::Locked;
            return Err(PairError::Locked);
        }
        Err(PairError::Unknown)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PairingInner> {
        // A panicking claim cannot leave the state unsafe to read — the
        // critical section has no failure point between mutations — so a
        // poisoned lock is recovered rather than propagated.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Draw [`PAIR_CODE_LEN`] symbols from the OS CSPRNG. `& 31` over a 32-symbol
/// alphabet is a bijection on the low 5 bits, so no rejection loop and no
/// modulo bias. Never derived from a timestamp or a counter.
fn mint_code() -> [u8; PAIR_CODE_LEN] {
    use rand::RngCore;
    let mut raw = [0u8; PAIR_CODE_LEN];
    rand::rngs::OsRng.fill_bytes(&mut raw);
    let mut code = [0u8; PAIR_CODE_LEN];
    for (out, byte) in code.iter_mut().zip(raw.iter()) {
        *out = PAIR_CODE_ALPHABET[(*byte & 31) as usize];
    }
    code
}

/// Normalise a submitted code to the printed alphabet, or reject it.
///
/// Case-insensitive (the printed form is upper case); surrounding whitespace
/// is tolerated because the code is pasted. Anything else — wrong length, a
/// character outside the alphabet (`I`, `L`, `O`, `U` included) — is
/// `pair_code_invalid`, NOT a guess.
fn normalize_code(submitted: &str) -> Option<[u8; PAIR_CODE_LEN]> {
    let trimmed = submitted.trim();
    if trimmed.len() != PAIR_CODE_LEN {
        return None;
    }
    let mut out = [0u8; PAIR_CODE_LEN];
    for (slot, ch) in out.iter_mut().zip(trimmed.bytes()) {
        let upper = ch.to_ascii_uppercase();
        if !PAIR_CODE_ALPHABET.contains(&upper) {
            return None;
        }
        *slot = upper;
    }
    Some(out)
}

/// Validate `--web-url`: an http(s) URL with a host. Returns the client
/// origin with any trailing `/` removed so the printed link has exactly one
/// slash before the query. `None` means "not a usable URL" — the caller
/// prints the two labelled lines instead of a broken link.
pub fn validate_web_url(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let parsed = url::Url::parse(raw).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    if parsed.host_str().unwrap_or("").is_empty() {
        return None;
    }
    let trimmed = raw.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_owned())
}

/// The startup lines for a freshly minted code: ONE ready link when
/// `--web-url` validated, else the origin and the code as two labelled
/// lines. Plain text (no ANSI) so the link is copy-pasteable verbatim.
pub fn pairing_startup_lines(
    web_url: Option<&str>,
    server_origin: &str,
    code: &str,
) -> Vec<String> {
    match web_url.and_then(validate_web_url) {
        Some(client_origin) => vec![format!(
            "Open the web client: {client_origin}/?octos={server_origin}&pair={code}"
        )],
        None => vec![
            format!("Server origin: {server_origin}"),
            format!("Pairing code: {code}"),
        ],
    }
}

/// The peer address of the connection, or `None` when the service was wired
/// without `ConnectInfo`.
///
/// `Option<ConnectInfo<_>>` is not an axum 0.8 extractor (that needs
/// `OptionalFromRequestParts`, which `ConnectInfo` does not implement), and a
/// bare `ConnectInfo` would REJECT with 500 on a connect-info-less service
/// instead of refusing with the contract's 404. This reads the extension
/// directly and can never fail, so "no peer" is a policy decision (not
/// loopback ⇒ 404) rather than a transport error.
#[derive(Debug, Clone, Copy)]
pub struct PeerAddr(pub Option<SocketAddr>);

impl<S: Send + Sync> FromRequestParts<S> for PeerAddr {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(PeerAddr(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|info| info.0),
        ))
    }
}

/// Headers a reverse proxy sets but a direct local client does not. A request
/// carrying one arrived over loopback only because a proxy forwarded it, so
/// it must never satisfy the loopback gate.
fn is_loopback_peer(remote_ip: Option<IpAddr>, headers: &HeaderMap) -> bool {
    let loopback = remote_ip.map(|ip| ip.is_loopback()).unwrap_or(false);
    loopback && !super::solo_auth::is_proxied(headers)
}

/// Resolve the pairing state for a request, or the 404 the contract mandates.
///
/// 404 (not 403) for a non-loopback peer, and 404 when this deployment has no
/// pairing state at all — a client reads that as "pairing not supported" and
/// falls back to the manual origin+token form.
///
/// A host-managed server (`octos serve --host-managed`) mints no code at
/// startup; its host enables one on demand, and the code exchanges for the
/// EXTERNAL token, never the host's.
fn resolve(
    state: &AppState,
    peer: PeerAddr,
    headers: &HeaderMap,
) -> Result<Arc<PairingState>, StatusCode> {
    if !is_loopback_peer(peer.0.map(|addr| addr.ip()), headers) {
        return Err(StatusCode::NOT_FOUND);
    }
    if let Some(host_managed) = &state.host_managed {
        return host_managed.pairing().ok_or(StatusCode::NOT_FOUND);
    }
    state.pairing.clone().ok_or(StatusCode::NOT_FOUND)
}

fn error_response(err: PairError) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(serde_json::json!({ "error": { "kind": err.kind() } })),
    )
        .into_response()
}

/// `GET /pair/info` — unauthenticated, loopback only.
///
/// Answers before any session exists and without touching a workspace: it
/// reads the pairing state and nothing else. No token, no code, no workspace
/// paths, nothing about sessions.
pub async fn pair_info(
    State(state): State<Arc<AppState>>,
    peer: PeerAddr,
    headers: HeaderMap,
) -> Response {
    let pairing = match resolve(&state, peer, &headers) {
        Ok(pairing) => pairing,
        Err(status) => return status.into_response(),
    };
    axum::Json(serde_json::json!({
        "product": "octos",
        "version": env!("CARGO_PKG_VERSION"),
        "pairing_required": pairing.pairing_required(),
        "server_origin": pairing.server_origin(),
    }))
    .into_response()
}

/// `POST /pair/claim` — unauthenticated, loopback only.
///
/// The body is read as bytes and parsed here (rather than through the `Json`
/// extractor) so that EVERY malformed body — bad JSON, missing field, wrong
/// length — answers with the contract's `pair_code_invalid` shape instead of
/// axum's own rejection text.
pub async fn pair_claim(
    State(state): State<Arc<AppState>>,
    peer: PeerAddr,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let pairing = match resolve(&state, peer, &headers) {
        Ok(pairing) => pairing,
        Err(status) => return status.into_response(),
    };
    // Host-managed: a browser may claim only from the host's configured
    // origin (the Origin header is a browser guard; the code is the control).
    if state.host_managed.is_some() {
        if let Some(origin) = headers.get(axum::http::header::ORIGIN) {
            let allowed = origin
                .to_str()
                .is_ok_and(|origin| state.appui_allowed_origins.iter().any(|a| a == origin));
            if !allowed {
                return StatusCode::FORBIDDEN.into_response();
            }
        }
    }

    // The body is never logged, at any level: it carries a credential.
    let submitted = serde_json::from_slice::<ClaimRequest>(&body)
        .ok()
        .map(|req| req.code);
    let Some(submitted) = submitted else {
        return error_response(PairError::Invalid);
    };

    let result = pairing.claim(&submitted);
    if state.host_managed.is_some() {
        // Audited, without the code: every claim of a host-shown code.
        let outcome = match &result {
            Ok(_) => serde_json::json!({ "claimed": true }),
            Err(error) => serde_json::json!({ "claimed": false, "error": error.kind() }),
        };
        if let Err(error) = super::admin_audit::record_admin_action(
            &state,
            None,
            "host.pairing.claim",
            "external",
            None,
            Some(outcome),
        ) {
            tracing::error!(%error, "could not audit a pairing claim");
        }
    }
    match result {
        Ok(token) => axum::Json(serde_json::json!({
            "token": token,
            "server_origin": pairing.server_origin(),
        }))
        .into_response(),
        Err(err) => error_response(err),
    }
}

#[derive(serde::Deserialize)]
struct ClaimRequest {
    code: String,
}
