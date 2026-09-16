//! HTTP-level suite for WEB-PAIRING-CONTRACT-5100 `## Server`.
//!
//! Registered from `pairing.rs` (`#[cfg(test)] #[path = ...] mod`), the same
//! append-only pattern the `ui_protocol_backend_*_tests.rs` modules use.
//!
//! The router-level cases drive a REAL loopback listener through
//! `into_make_service_with_connect_info` so `ConnectInfo` — the loopback gate's
//! only input — is populated exactly as it is in production. The non-loopback
//! cases call the handlers directly with a fabricated `ConnectInfo`, because a
//! test cannot portably bind a public address.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};

use super::{PairingState, PeerAddr, pair_claim, pair_info};
use crate::api::{AppState, build_router};

const ORIGIN: &str = "http://127.0.0.1:50080";
const TOKEN: &str = "octos-api-token-for-tests";

fn state_with(pairing: Option<Arc<PairingState>>) -> Arc<AppState> {
    Arc::new(AppState {
        pairing,
        ..AppState::empty_for_tests()
    })
}

fn paired_state() -> (Arc<AppState>, String) {
    let pairing = Arc::new(PairingState::mint(ORIGIN, Some(TOKEN.to_owned())));
    let code = pairing.printed_code();
    (state_with(Some(pairing)), code)
}

/// Spin the real router on an ephemeral loopback port WITH connect info.
async fn serve(state: Arc<AppState>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    tokio::task::yield_now().await;
    (addr, handle)
}

async fn claim(addr: SocketAddr, body: &str) -> (StatusCode, serde_json::Value) {
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/pair/claim"))
        .header("content-type", "application/json")
        .body(body.to_owned())
        .send()
        .await
        .unwrap();
    let status = response.status();
    let json = response
        .json::<serde_json::Value>()
        .await
        .unwrap_or(serde_json::Value::Null);
    (status, json)
}

fn code_body(code: &str) -> String {
    serde_json::json!({ "code": code }).to_string()
}

fn error_kind(json: &serde_json::Value) -> &str {
    json["error"]["kind"].as_str().unwrap_or("<missing>")
}

// ── GET /pair/info ────────────────────────────────────────────────────────

#[tokio::test]
async fn pair_info_answers_the_contract_shape_before_any_session_exists() {
    let (state, _code) = paired_state();
    // `empty_for_tests` has no SessionManager, no profile runtime and no
    // workspace — `/pair/info` must still answer.
    assert!(state.sessions.is_none());
    assert!(state.profiles.is_empty());
    let (addr, server) = serve(state).await;

    let response = reqwest::get(format!("http://{addr}/pair/info"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await.unwrap();

    assert_eq!(body["product"], "octos");
    assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(body["pairing_required"], true);
    assert_eq!(body["server_origin"], ORIGIN);

    let keys: Vec<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys.len(),
        4,
        "no token, no code, no workspace paths, nothing about sessions: {body}"
    );
    let serialized = body.to_string();
    assert!(!serialized.contains(TOKEN), "info must not leak the token");

    server.abort();
}

#[tokio::test]
async fn pair_info_reports_pairing_not_required_when_the_server_has_no_token() {
    let state = state_with(Some(Arc::new(PairingState::mint(ORIGIN, None))));
    let (addr, server) = serve(state).await;

    let body: serde_json::Value = reqwest::get(format!("http://{addr}/pair/info"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["pairing_required"], false);

    server.abort();
}

#[tokio::test]
async fn pairing_endpoints_are_404_when_the_deployment_has_no_pairing_state() {
    // A server without pairing must read as "pairing not supported" (404), so
    // the client falls back to the manual origin+token form.
    let (addr, server) = serve(state_with(None)).await;

    let info = reqwest::get(format!("http://{addr}/pair/info"))
        .await
        .unwrap();
    assert_eq!(info.status(), StatusCode::NOT_FOUND);
    let (status, _) = claim(addr, &code_body("3QK7ZP2M")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    server.abort();
}

// ── POST /pair/claim ──────────────────────────────────────────────────────

#[tokio::test]
async fn pair_claim_returns_the_token_then_pair_code_unknown_on_the_second_claim() {
    let (state, code) = paired_state();
    let (addr, server) = serve(state).await;

    let (status, body) = claim(addr, &code_body(&code)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["token"], TOKEN);
    assert_eq!(body["server_origin"], ORIGIN);

    // Single use: the first success burned the code.
    let (status, body) = claim(addr, &code_body(&code)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_kind(&body), "pair_code_unknown");
    assert!(body.get("token").is_none(), "no token on an error: {body}");

    server.abort();
}

#[tokio::test]
async fn pair_claim_accepts_the_printed_alphabet_case_insensitively() {
    let (state, code) = paired_state();
    let (addr, server) = serve(state).await;

    let (status, body) = claim(addr, &code_body(&code.to_ascii_lowercase())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["token"], TOKEN);

    server.abort();
}

#[tokio::test]
async fn pair_claim_answers_pair_code_expired_after_the_window() {
    let pairing = Arc::new(PairingState::mint_with_ttl(
        ORIGIN,
        Some(TOKEN.to_owned()),
        Duration::ZERO,
    ));
    let code = pairing.printed_code();
    let (addr, server) = serve(state_with(Some(pairing))).await;

    let (status, body) = claim(addr, &code_body(&code)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_kind(&body), "pair_code_expired");

    server.abort();
}

#[tokio::test]
async fn pair_claim_malformed_body_is_invalid_and_does_not_consume_the_budget() {
    let (state, code) = paired_state();
    let (addr, server) = serve(state).await;

    for body in [
        "",
        "not json at all",
        "{}",
        r#"{"code": 12345678}"#,
        r#"{"code": "short"}"#,
        r#"{"code": "IIIIIIII"}"#,
        r#"{"pair": "3QK7ZP2M"}"#,
    ] {
        let (status, json) = claim(addr, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body {body:?}");
        assert_eq!(error_kind(&json), "pair_code_invalid", "body {body:?}");
    }

    // 7 malformed bodies — more than half the budget — and the code still
    // works: a malformed body is not a guess.
    let (status, json) = claim(addr, &code_body(&code)).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_eq!(json["token"], TOKEN);

    server.abort();
}

#[tokio::test]
async fn pair_claim_burns_the_code_after_ten_failed_attempts() {
    let (state, code) = paired_state();
    let (addr, server) = serve(state).await;

    let first = code.as_bytes()[0];
    let wrong = format!("{}{}", if first == b'0' { '1' } else { '0' }, &code[1..]);

    for attempt in 1..super::PAIR_MAX_FAILED_CLAIMS {
        let (status, json) = claim(addr, &code_body(&wrong)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(error_kind(&json), "pair_code_unknown", "attempt {attempt}");
    }
    // The 10th failure spends the budget and burns the code.
    let (_, json) = claim(addr, &code_body(&wrong)).await;
    assert_eq!(error_kind(&json), "pair_code_locked");
    // Even the correct code is locked out now.
    let (status, json) = claim(addr, &code_body(&code)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error_kind(&json), "pair_code_locked");

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_claims_over_http_hand_out_exactly_one_token() {
    let (state, code) = paired_state();
    let (addr, server) = serve(state).await;

    let barrier = Arc::new(tokio::sync::Barrier::new(16));
    let mut handles = Vec::new();
    for _ in 0..16 {
        let body = code_body(&code);
        let barrier = Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            claim(addr, &body).await
        }));
    }

    let mut successes = 0;
    for handle in handles {
        let (status, json) = handle.await.unwrap();
        if status == StatusCode::OK {
            assert_eq!(json["token"], TOKEN);
            successes += 1;
        } else {
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(error_kind(&json), "pair_code_unknown");
        }
    }
    assert_eq!(successes, 1, "the token must be handed out exactly once");

    server.abort();
}

// ── loopback gate ─────────────────────────────────────────────────────────

/// A non-loopback peer gets 404 — not 403 — on BOTH endpoints, so the
/// endpoints never confirm the server exists to the wider network.
#[tokio::test]
async fn non_loopback_peer_gets_404_on_both_endpoints() {
    let (state, code) = paired_state();
    let remote: SocketAddr = "203.0.113.7:44321".parse().unwrap();

    let info = pair_info(
        State(Arc::clone(&state)),
        PeerAddr(Some(remote)),
        HeaderMap::new(),
    )
    .await;
    assert_eq!(info.status(), StatusCode::NOT_FOUND);

    let claimed = pair_claim(
        State(Arc::clone(&state)),
        PeerAddr(Some(remote)),
        HeaderMap::new(),
        code_body(&code).into(),
    )
    .await;
    assert_eq!(claimed.status(), StatusCode::NOT_FOUND);

    // The refused claim must not have burned the code: a loopback client can
    // still pair afterwards.
    assert!(state.pairing.as_ref().unwrap().claim(&code).is_ok());
}

/// A loopback request carrying proxy headers was forwarded by a reverse proxy
/// (the Caddy-fronted fleet shape) and must not launder itself through the
/// loopback gate.
#[tokio::test]
async fn proxied_loopback_request_gets_404_on_both_endpoints() {
    let (state, code) = paired_state();
    let local: SocketAddr = "127.0.0.1:44321".parse().unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("x-forwarded-for", "203.0.113.7".parse().unwrap());

    let info = pair_info(
        State(Arc::clone(&state)),
        PeerAddr(Some(local)),
        headers.clone(),
    )
    .await;
    assert_eq!(info.status(), StatusCode::NOT_FOUND);

    let claimed = pair_claim(
        State(Arc::clone(&state)),
        PeerAddr(Some(local)),
        headers,
        code_body(&code).into(),
    )
    .await;
    assert_eq!(claimed.status(), StatusCode::NOT_FOUND);
}

/// A request with no `ConnectInfo` at all (a service wired without connect
/// info) cannot prove it is local, so it is refused too.
#[tokio::test]
async fn request_without_connect_info_gets_404() {
    let (state, _code) = paired_state();
    let info = pair_info(State(state), PeerAddr(None), HeaderMap::new()).await;
    assert_eq!(info.status(), StatusCode::NOT_FOUND);
}

// ── pairing never bypasses bearer auth ────────────────────────────────────

/// Pairing hands out the bearer token; it does not weaken the middleware that
/// checks it. An unauthenticated request to a protected route is still 401
/// before AND after a successful claim.
#[tokio::test]
async fn pairing_does_not_bypass_bearer_auth_on_protected_routes() {
    let pairing = Arc::new(PairingState::mint(ORIGIN, Some(TOKEN.to_owned())));
    let code = pairing.printed_code();
    let state = Arc::new(AppState {
        pairing: Some(pairing),
        auth_token: Some(TOKEN.to_owned()),
        ..AppState::empty_for_tests()
    });
    let (addr, server) = serve(state).await;
    let client = reqwest::Client::new();

    let unauthenticated = client
        .get(format!("http://{addr}/metrics"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    let (status, json) = claim(addr, &code_body(&code)).await;
    assert_eq!(status, StatusCode::OK);
    let token = json["token"].as_str().unwrap().to_owned();

    // Still 401 without the header...
    let still_unauthenticated = client
        .get(format!("http://{addr}/metrics"))
        .send()
        .await
        .unwrap();
    assert_eq!(still_unauthenticated.status(), StatusCode::UNAUTHORIZED);

    // ... and the paired token is the one the existing middleware accepts.
    let authenticated = client
        .get(format!("http://{addr}/metrics"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(authenticated.status(), StatusCode::OK);

    server.abort();
}
