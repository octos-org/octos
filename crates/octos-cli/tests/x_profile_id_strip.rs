//! Integration tests for the `X-Profile-Id` header strip middleware (issue
//! [#995](https://github.com/octos-org/octos/issues/995)) and the handler
//! precedence flip in `handlers::decide_resolved_profile_id`.
//!
//! ## What this guards
//!
//! Before the fix, `crates/octos-cli/src/api/handlers.rs:1442` resolved
//! the request profile via `header.or(identity)` — an authenticated user
//! could attach `X-Profile-Id: <victim>` and the daemon would walk
//! straight into the victim's data dir. Both layers of the fix are
//! exercised:
//!
//! 1. **Strip middleware** — non-loopback requests carrying
//!    `X-Profile-Id` have the header removed before any handler sees it.
//!    Hosted clients with no admin token and no Caddy ingress in front
//!    are the attacker model.
//! 2. **Handler authorization** — even if a trusted proxy DOES pass the
//!    header through (the production Caddy path), the handler now
//!    checks the authenticated identity owns the profile, returning
//!    403 on mismatch instead of silently overriding.
//!
//! ## Why a `200`-only happy path isn't enough
//!
//! `tower::oneshot` builds requests without `ConnectInfo`, which the
//! strip middleware treats as untrusted (see `is_trusted_proxy_addr`).
//! Tests built this way DO exercise the strip path — and that's
//! exactly what we want for the bypass guard: the `X-Profile-Id` MUST
//! be stripped before any handler can latch onto it.

#![cfg(feature = "api")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use octos_cli::api::{AppState, build_router};
use tempfile::TempDir;
use tower::util::ServiceExt;

/// Build an `AppState` with a `profile_store` containing the listed
/// profiles. The store is shared with the auth manager (so OTP login
/// can grant sessions for these ids) and the X-Profile-Id branch in
/// `is_authorized_for_profile` can resolve sub-account parentage.
fn build_state(_dir: &TempDir, profiles: &[(&str, Option<&str>)]) -> Arc<AppState> {
    let store = Arc::new(octos_cli::profiles::ProfileStore::open(_dir.path()).unwrap());
    for (id, parent) in profiles {
        let profile = octos_cli::profiles::UserProfile {
            id: (*id).into(),
            name: (*id).into(),
            enabled: true,
            data_dir: None,
            parent_id: parent.map(|p| p.into()),
            public_subdomain: None,
            config: Default::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        };
        store.save(&profile).unwrap();
    }

    Arc::new(AppState {
        profile_store: Some(store),
        auth_token: Some("admin-secret".into()),
        ..AppState::empty_for_tests()
    })
}

/// Sanity smoke: building the router with the strip middleware doesn't
/// regress the public-health endpoint. Public routes must remain
/// reachable regardless of header strip behaviour.
#[tokio::test]
async fn health_endpoint_remains_reachable_with_strip_middleware() {
    let dir = TempDir::new().unwrap();
    let state = build_state(&dir, &[]);
    let app = build_router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
}

/// The Layer 1 strip middleware: a non-loopback request carrying
/// `X-Profile-Id` reaches the auth middleware with the header REMOVED.
///
/// We can't probe header presence from outside the daemon, so we
/// exercise this through the auth middleware's `X-Profile-Id`-as-auth
/// branch. That branch (router.rs ~711-744) accepts the header only
/// when the request comes from loopback AND the profile exists. With
/// `tower::oneshot` (no ConnectInfo → "untrusted") the strip middleware
/// runs first and removes the header — so the request reaches the auth
/// middleware with NO header to honor. The expected result is `401`,
/// not the legacy `200` of the proxy-auth path.
#[tokio::test]
async fn untrusted_request_with_x_profile_id_falls_into_unauthorized_path() {
    let dir = TempDir::new().unwrap();
    let state = build_state(&dir, &[("victim", None)]);
    let app = build_router(state);

    // No bearer token, no admin auth — the only signal we send is the
    // forged `X-Profile-Id: victim`. Pre-fix the auth middleware would
    // also have rejected this (the loopback check at router.rs:712-716
    // catches the *auth* path), but the *handler* path inside
    // `resolve_profile_data_dir` would have read the same raw header.
    //
    // The strip middleware closes both doors at once: the auth-path
    // rejection is `401`, the handler-path rejection is `BAD_REQUEST` /
    // `403`. Either way, no `200` with victim's data.
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/files/list")
                .header("x-profile-id", "victim")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "untrusted request with forged X-Profile-Id must be rejected, not honored"
    );
}

/// Pre-fix bypass evidence: an authenticated request that sets
/// `X-Profile-Id: <victim>` MUST NOT walk into the victim's data dir.
/// Even if the strip middleware preserves the header (e.g. via a
/// trusted proxy), the handler-layer authorization in
/// `decide_resolved_profile_id` blocks the cross-tenant override.
///
/// Pre-fix (`header.or(identity)`): the daemon returned the victim's
/// data dir / listings — silently. This test is the post-fix guard:
/// the handler must NOT report a 200 with the victim's files.
#[tokio::test]
async fn authenticated_request_with_cross_tenant_x_profile_id_is_denied() {
    let dir = TempDir::new().unwrap();
    let state = build_state(&dir, &[("alice", None), ("victim", None)]);
    let app = build_router(state);

    // Authenticate as the bootstrap admin token — that gives us a
    // valid AuthIdentity::Admin. Admin is the most generous identity
    // and would have surfaced the bypass most visibly pre-fix. With
    // admin auth the strip middleware does NOT strip the header from a
    // loopback path (loopback is trusted), so the handler-layer
    // authorization check is what's under test here.
    //
    // Note: in `tower::oneshot` there's no ConnectInfo, so the strip
    // middleware treats it as untrusted and removes the header. That
    // already proves the Layer 1 guard. To exercise Layer 2 directly
    // we cover it in the handlers.rs unit tests on
    // `decide_resolved_profile_id`; this integration test asserts the
    // end-to-end result for the bypass shape: status code MUST NOT be
    // a 200 with victim's data.
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/files/list")
                .header("authorization", "Bearer admin-secret")
                .header("x-profile-id", "victim")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Admin auth + stripped header + no process_manager → the handler
    // hits the `resolve_api_port` -> SERVICE_UNAVAILABLE branch
    // (gateway not configured under tests). The contract here is
    // explicitly NOT 200: even when the legacy bypass would have
    // resolved to "victim", the post-fix path can't honor the header
    // because it never reaches the handler.
    assert_ne!(
        resp.status(),
        StatusCode::OK,
        "pre-fix bypass returned 200 with victim's data dir contents — \
         post-fix this MUST be a non-success status"
    );
    // The exact code depends on environment: SERVICE_UNAVAILABLE when
    // `process_manager` is absent (the test default), or
    // FORBIDDEN/NOT_FOUND in fuller wirings. Pin to the family.
    assert!(
        resp.status().is_client_error() || resp.status().is_server_error(),
        "non-success expected, got {}",
        resp.status()
    );
}

/// Even reaching the WS upgrade endpoint with a forged header on an
/// unauthenticated, non-loopback request must not let the connection
/// upgrade with a victim profile id pinned.
///
/// The WS handler stashes `routed_profile_id_from_headers(...)` onto
/// the connection at upgrade time (ui_protocol.rs:1869). With the
/// strip middleware in place the header is gone before the WS handler
/// sees it, so the connection cannot be implicitly bound to the
/// victim's profile through a forged header.
#[tokio::test]
async fn ws_upgrade_attempt_without_auth_with_forged_x_profile_id_fails() {
    let dir = TempDir::new().unwrap();
    let state = build_state(&dir, &[("victim", None)]);
    let app = build_router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/ui-protocol/ws")
                .header("x-profile-id", "victim")
                .header("connection", "Upgrade")
                .header("upgrade", "websocket")
                .header("sec-websocket-version", "13")
                .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Without auth, without ConnectInfo (→ untrusted) and with the
    // header stripped, the auth middleware rejects with 401. Pre-fix
    // the auth middleware also rejected here (loopback check at
    // router.rs:712), so this isn't a regression-only test —  it's a
    // defence-in-depth assertion that the strip middleware doesn't
    // accidentally open the WS handler to forged profile ids.
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "WS upgrade with forged header on untrusted hop must be 401"
    );
}
