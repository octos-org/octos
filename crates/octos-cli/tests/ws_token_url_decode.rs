//! Integration tests for [issue #1010](https://github.com/octos-org/octos/issues/1010):
//! WS token auth must percent-decode the `?token=` query parameter so
//! browsers / curl can pass tokens that contain `!`, `+`, `/`, `=`, `:`
//! etc. without silent 401s.
//!
//! ## What this guards
//!
//! `extract_token` in `crates/octos-cli/src/api/router.rs` used to take
//! the raw query string value and hand it to `resolve_identity`
//! verbatim. Browsers and curl percent-encode special characters when
//! they build the URL, so a token like
//! `Octos-E2E-Strong-Token-2026-XYZ-123!` arrived at the server as
//! `…123%21` and never matched the stored
//! `AppState.auth_token = "…123!"`. REST callers were unaffected
//! because the `Authorization: Bearer …` header path takes bytes
//! literally (HTTP header values are NOT percent-encoded).
//!
//! ## How the assertions work
//!
//! `tower::oneshot` does not attach the `hyper::upgrade::OnUpgrade`
//! extension, so a real WS upgrade is impossible in this harness. The
//! contract we test is therefore "did the auth middleware accept or
//! reject this token?":
//!
//! * Auth accepted ⇒ control reaches the WS handler ⇒ the missing
//!   upgrade extension causes axum's `WebSocketUpgradeRejection`
//!   path to return a **4xx that is NOT 401** (today: `400 Bad
//!   Request` from `MethodNotGet`/`InvalidConnectionHeader` etc., or
//!   `426 Upgrade Required`). The test only asserts `status != 401`,
//!   so future axum versions that change the rejection status code
//!   do not break this guard.
//! * Auth rejected ⇒ the middleware short-circuits with **`401
//!   Unauthorized`** before the handler runs.
//!
//! Pre-fix, every "auth accepted" assertion below failed with `401`
//! because `extract_token` returned the percent-encoded literal.

#![cfg(feature = "api")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use octos_cli::api::{AppState, build_router};
use tempfile::TempDir;
use tower::util::ServiceExt;

/// Build an `AppState` with a single stored admin token. The token is
/// the literal pre-decoded value — exactly what a REST caller would
/// pass in `Authorization: Bearer …`. The WS path must arrive at this
/// same byte sequence after percent-decoding the `?token=` value.
fn build_state_with_admin_token(_dir: &TempDir, token: &str) -> Arc<AppState> {
    let store = Arc::new(octos_cli::profiles::ProfileStore::open(_dir.path()).unwrap());
    Arc::new(AppState {
        profile_store: Some(store),
        auth_token: Some(token.into()),
        ..AppState::empty_for_tests()
    })
}

/// Issue #1010 — `!` is a sub-delim per RFC 3986 and browsers /
/// `URLSearchParams.toString()` encode it to `%21` in query values.
/// Pre-fix the server compared `"test-token-%21"` against the stored
/// `"test-token-!"` and returned 401.
#[tokio::test]
async fn should_authenticate_ws_with_token_containing_exclamation() {
    let dir = TempDir::new().unwrap();
    let state = build_state_with_admin_token(&dir, "test-token-!");
    let app = build_router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/ui-protocol/ws?token=test-token-%21")
                .header("connection", "Upgrade")
                .header("upgrade", "websocket")
                .header("sec-websocket-version", "13")
                .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_ne!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "WS upgrade with percent-encoded `!` token must NOT be rejected \
         by the auth middleware — see issue #1010"
    );
}

/// Issue #1010 — `+` and `%` ambiguity test.
///
/// `+` is a reserved-ish character with two competing decode rules:
/// `application/x-www-form-urlencoded` decodes `+` → space, while RFC
/// 3986 percent-decoding leaves `+` literal. The REST `Authorization:
/// Bearer …` path is literal (no decoding at all), so the WS path
/// MUST use RFC 3986 semantics — otherwise REST and WS accept
/// different token formats and operators get confused.
///
/// Query value: `test+token%2Bfoo`
///   * `+` stays literal `+`
///   * `%2B` decodes to `+`
///   * result: `test+token+foo`
///
/// Pre-fix the server compared `"test+token%2Bfoo"` against the stored
/// `"test+token+foo"` and returned 401.
#[tokio::test]
async fn should_authenticate_ws_with_token_containing_plus_and_percent() {
    let dir = TempDir::new().unwrap();
    // Decoded form (what REST `Authorization: Bearer …` would receive).
    let stored_token = "test+token+foo";
    let state = build_state_with_admin_token(&dir, stored_token);
    let app = build_router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/ui-protocol/ws?token=test+token%2Bfoo")
                .header("connection", "Upgrade")
                .header("upgrade", "websocket")
                .header("sec-websocket-version", "13")
                .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_ne!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "WS upgrade with mixed `+` (literal) and `%2B` (decodes to `+`) \
         must NOT be rejected — the WS path uses RFC 3986 percent-decode \
         semantics so `+` stays literal, matching the REST header path \
         which never decodes anything"
    );
}

/// Regression — a plain alphanumeric token continues to authenticate.
/// Guards against an over-eager decoder that mangles ASCII letters or
/// numbers, or an off-by-one that drops a character on every request.
#[tokio::test]
async fn should_authenticate_ws_with_alphanumeric_token() {
    let dir = TempDir::new().unwrap();
    let state = build_state_with_admin_token(&dir, "simpleToken123");
    let app = build_router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/ui-protocol/ws?token=simpleToken123")
                .header("connection", "Upgrade")
                .header("upgrade", "websocket")
                .header("sec-websocket-version", "13")
                .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_ne!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "WS upgrade with a plain alphanumeric token must continue to \
         authenticate — pre-#1010 behaviour preserved"
    );
}

/// Issue #1010 inverse — passing a URL-encoded version of a WRONG
/// token must STILL return 401. The fix must not accidentally accept
/// any token whose decoded form differs from the stored value.
///
/// Stored token: `correct-token!`. Client supplies `wrong-token%21`
/// (`wrong-token!` after decoding) — still wrong, still 401.
#[tokio::test]
async fn should_reject_ws_with_wrong_decoded_token() {
    let dir = TempDir::new().unwrap();
    let state = build_state_with_admin_token(&dir, "correct-token!");
    let app = build_router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/ui-protocol/ws?token=wrong-token%21")
                .header("connection", "Upgrade")
                .header("upgrade", "websocket")
                .header("sec-websocket-version", "13")
                .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "WS upgrade with a wrong token (even after percent-decoding) \
         must continue to 401 — the fix MUST NOT widen the accept set"
    );
}
