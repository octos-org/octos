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
            }),
            minted_at: Instant::now(),
            ttl,
            token: token.unwrap_or_default(),
            server_origin: server_origin.into(),
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

    #[cfg(test)]
    fn failures(&self) -> u32 {
        self.lock().failures
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
fn resolve<'a>(
    state: &'a AppState,
    peer: PeerAddr,
    headers: &HeaderMap,
) -> Result<&'a Arc<PairingState>, StatusCode> {
    if !is_loopback_peer(peer.0.map(|addr| addr.ip()), headers) {
        return Err(StatusCode::NOT_FOUND);
    }
    state.pairing.as_ref().ok_or(StatusCode::NOT_FOUND)
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

    // The body is never logged, at any level: it carries a credential.
    let submitted = serde_json::from_slice::<ClaimRequest>(&body)
        .ok()
        .map(|req| req.code);
    let Some(submitted) = submitted else {
        return error_response(PairError::Invalid);
    };

    match pairing.claim(&submitted) {
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

#[cfg(test)]
#[path = "pairing_tests.rs"]
mod pairing_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn live_state(token: &str) -> PairingState {
        PairingState::mint("http://127.0.0.1:50080", Some(token.to_owned()))
    }

    #[test]
    fn should_mint_eight_crockford_characters_when_minted() {
        let state = live_state("tok");
        let code = state.printed_code();
        assert_eq!(code.len(), PAIR_CODE_LEN);
        for ch in code.bytes() {
            assert!(
                PAIR_CODE_ALPHABET.contains(&ch),
                "code character {ch:?} is outside the Crockford alphabet"
            );
        }
        for ambiguous in ['I', 'L', 'O', 'U'] {
            assert!(!code.contains(ambiguous), "ambiguous letter in {code}");
        }
    }

    #[test]
    fn should_mint_distinct_codes_when_called_repeatedly() {
        // A timestamp- or counter-derived code would collide or march; 20
        // draws from a 32^8 space must not repeat.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..20 {
            assert!(seen.insert(live_state("tok").printed_code()));
        }
    }

    #[test]
    fn should_return_token_when_first_claim_matches() {
        let state = live_state("api-token-xyz");
        let code = state.printed_code();
        assert_eq!(state.claim(&code), Ok("api-token-xyz".to_owned()));
    }

    #[test]
    fn should_accept_lowercase_and_padded_code_when_claiming() {
        let state = live_state("api-token-xyz");
        let code = state.printed_code();
        let submitted = format!("  {}  ", code.to_ascii_lowercase());
        assert_eq!(state.claim(&submitted), Ok("api-token-xyz".to_owned()));
    }

    #[test]
    fn should_fail_pair_code_unknown_when_claimed_twice() {
        let state = live_state("api-token-xyz");
        let code = state.printed_code();
        assert!(state.claim(&code).is_ok());
        assert_eq!(state.claim(&code), Err(PairError::Unknown));
        assert_eq!(PairError::Unknown.kind(), "pair_code_unknown");
    }

    #[test]
    fn should_fail_pair_code_expired_when_past_ttl() {
        let state =
            PairingState::mint_with_ttl("http://127.0.0.1:1", Some("t".into()), Duration::ZERO);
        let code = state.printed_code();
        assert_eq!(state.claim(&code), Err(PairError::Expired));
        assert_eq!(PairError::Expired.kind(), "pair_code_expired");
    }

    #[test]
    fn should_not_consume_attempt_budget_when_code_is_malformed() {
        let state = live_state("api-token-xyz");
        for malformed in [
            "",
            "short",
            "toolongcode",
            "IIIIIIII",
            "1234567!",
            "OOOOOOOO",
        ] {
            assert_eq!(
                state.claim(malformed),
                Err(PairError::Invalid),
                "{malformed:?} must be pair_code_invalid"
            );
        }
        assert_eq!(state.failures(), 0, "malformed bodies are not guesses");
        // ... and the code still works afterwards.
        let code = state.printed_code();
        assert!(state.claim(&code).is_ok());
    }

    #[test]
    fn should_burn_code_when_attempt_budget_is_exhausted() {
        let state = live_state("api-token-xyz");
        let code = state.printed_code();
        let wrong = wrong_code(&code);
        for attempt in 1..PAIR_MAX_FAILED_CLAIMS {
            assert_eq!(
                state.claim(&wrong),
                Err(PairError::Unknown),
                "failure {attempt} is still within budget"
            );
        }
        // The 10th failure spends the budget and burns the code.
        assert_eq!(state.claim(&wrong), Err(PairError::Locked));
        // Every later claim — including the CORRECT code — is locked.
        assert_eq!(state.claim(&code), Err(PairError::Locked));
        assert_eq!(PairError::Locked.kind(), "pair_code_locked");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn should_yield_exactly_one_success_when_claims_race() {
        for _ in 0..32 {
            let state = Arc::new(live_state("api-token-xyz"));
            let code = state.printed_code();
            let barrier = Arc::new(tokio::sync::Barrier::new(16));
            let mut handles = Vec::new();
            for _ in 0..16 {
                let state = Arc::clone(&state);
                let code = code.clone();
                let barrier = Arc::clone(&barrier);
                handles.push(tokio::spawn(async move {
                    barrier.wait().await;
                    state.claim(&code)
                }));
            }
            let mut successes = 0;
            for handle in handles {
                match handle.await.unwrap() {
                    Ok(token) => {
                        assert_eq!(token, "api-token-xyz");
                        successes += 1;
                    }
                    Err(err) => assert_eq!(err, PairError::Unknown),
                }
            }
            assert_eq!(successes, 1, "the token must be handed out exactly once");
        }
    }

    /// A well-formed code that is guaranteed to differ from `code`.
    fn wrong_code(code: &str) -> String {
        let first = code.as_bytes()[0];
        let replacement = if first == b'0' { '1' } else { '0' };
        format!("{replacement}{}", &code[1..])
    }

    #[test]
    fn should_reject_non_http_web_url_when_validating() {
        for bad in [
            "",
            "   ",
            "not a url",
            "example.com",
            "ftp://example.com",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "/",
        ] {
            assert_eq!(validate_web_url(bad), None, "{bad:?} must not validate");
        }
    }

    #[test]
    fn should_normalise_trailing_slash_when_validating_web_url() {
        assert_eq!(
            validate_web_url("https://app.example.com/"),
            Some("https://app.example.com".to_owned())
        );
        assert_eq!(
            validate_web_url("  http://localhost:5173  "),
            Some("http://localhost:5173".to_owned())
        );
        assert_eq!(
            validate_web_url("https://app.example.com/web/"),
            Some("https://app.example.com/web".to_owned())
        );
    }

    #[test]
    fn should_print_one_link_when_web_url_is_valid() {
        let lines = pairing_startup_lines(
            Some("https://app.example.com/"),
            "http://127.0.0.1:50080",
            "3QK7ZP2M",
        );
        assert_eq!(
            lines,
            vec![
                "Open the web client: https://app.example.com/?octos=http://127.0.0.1:50080&pair=3QK7ZP2M"
                    .to_owned()
            ]
        );
    }

    #[test]
    fn should_print_two_labelled_lines_when_web_url_is_absent_or_broken() {
        let expected = vec![
            "Server origin: http://127.0.0.1:50080".to_owned(),
            "Pairing code: 3QK7ZP2M".to_owned(),
        ];
        assert_eq!(
            pairing_startup_lines(None, "http://127.0.0.1:50080", "3QK7ZP2M"),
            expected
        );
        assert_eq!(
            pairing_startup_lines(Some("not a url"), "http://127.0.0.1:50080", "3QK7ZP2M"),
            expected,
            "a broken --web-url must degrade to the two labelled lines"
        );
    }

    #[test]
    fn should_reject_proxied_or_remote_peer_when_gating_loopback() {
        let empty = HeaderMap::new();
        assert!(is_loopback_peer(Some("127.0.0.1".parse().unwrap()), &empty));
        assert!(is_loopback_peer(Some("::1".parse().unwrap()), &empty));
        assert!(!is_loopback_peer(
            Some("203.0.113.7".parse().unwrap()),
            &empty
        ));
        assert!(!is_loopback_peer(
            Some("2001:db8::1".parse().unwrap()),
            &empty
        ));
        assert!(!is_loopback_peer(None, &empty), "no peer ⇒ not loopback");

        let mut proxied = HeaderMap::new();
        proxied.insert("x-forwarded-for", "203.0.113.7".parse().unwrap());
        assert!(
            !is_loopback_peer(Some("127.0.0.1".parse().unwrap()), &proxied),
            "a forwarded request must not launder itself through loopback"
        );
    }
}
