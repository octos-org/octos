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

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::Extension;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
use octos_cli::api::{
    AppState, TestAuthIdentity, TestSessionMessagesPaginationParams, build_router,
    test_session_files, test_session_messages, test_session_workspace_contract,
};
use tempfile::TempDir;
use tower::util::ServiceExt;

/// Loopback `SocketAddr` for tests that need a TRUSTED hop (Caddy
/// ingress is `127.0.0.1:NN` in production). `is_trusted_proxy_addr`
/// returns `true` for any address whose `is_loopback()` is `true`,
/// without consulting `OCTOS_TRUSTED_PROXY_CIDRS`.
fn loopback_socket_addr() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 65432)
}

/// External `SocketAddr` for tests that need an UNTRUSTED hop. Cloudflare's
/// `1.1.1.1` is outside any default-trusted block.
fn external_socket_addr() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 4444)
}

/// Inject a `ConnectInfo<SocketAddr>` extension into the request. axum
/// normally wires this up via `into_make_service_with_connect_info`,
/// but for `tower::ServiceExt::oneshot` we have to attach the value
/// manually so the strip middleware + auth middleware see a remote IP.
fn with_connect_info(mut req: Request<Body>, addr: SocketAddr) -> Request<Body> {
    req.extensions_mut().insert(ConnectInfo(addr));
    req
}

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

/// Build an `AppState` wired with a real `AuthManager` + `UserStore`
/// + `ProfileStore`. Used by the trusted-hop tests that need a
/// non-admin authenticated identity. `users` describes
/// `(user_id, email, role)`, and `profiles` describes
/// `(profile_id, parent_id)` matching the `build_state` helper.
fn build_state_with_users(
    dir: &TempDir,
    users: &[(&str, &str, octos_cli::user_store::UserRole)],
    profiles: &[(&str, Option<&str>)],
) -> (Arc<AppState>, Arc<octos_cli::otp::AuthManager>) {
    let profile_store = Arc::new(octos_cli::profiles::ProfileStore::open(dir.path()).unwrap());
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
        profile_store.save(&profile).unwrap();
    }

    let user_store = Arc::new(octos_cli::user_store::UserStore::open(dir.path()).unwrap());
    for (id, email, role) in users {
        let user = octos_cli::user_store::User {
            id: (*id).into(),
            email: (*email).into(),
            name: (*id).into(),
            role: role.clone(),
            created_at: chrono::Utc::now(),
            last_login_at: None,
        };
        user_store.save(&user).unwrap();
    }

    // Use a static token so we don't need an SMTP send round-trip to
    // mint a session.
    let auth_config = octos_cli::otp::DashboardAuthConfig {
        smtp: octos_cli::otp::SmtpConfig {
            host: "unused".into(),
            port: 587,
            username: "unused@example.com".into(),
            password_env: "UNUSED_PASSWORD".into(),
            from_address: "unused@example.com".into(),
        },
        session_expiry_hours: 24,
        allow_self_registration: false,
        static_tokens: vec!["e2e-static-bypass".into()],
    };
    let auth_manager = Arc::new(octos_cli::otp::AuthManager::new(
        Some(auth_config),
        user_store.clone(),
    ));

    let state = Arc::new(AppState {
        profile_store: Some(profile_store),
        user_store: Some(user_store),
        auth_manager: Some(auth_manager.clone()),
        auth_token: Some("admin-secret".into()),
        ..AppState::empty_for_tests()
    });
    (state, auth_manager)
}

/// Mint a session token for `email` via the configured static-token
/// bypass. Panics if the static token is not configured on the auth
/// manager (test setup bug).
async fn mint_session_token(mgr: &octos_cli::otp::AuthManager, email: &str) -> String {
    mgr.verify_otp_with_registration(email, "e2e-static-bypass", false)
        .await
        .expect("verify must succeed under static-token bypass")
        .expect("session token must be issued")
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

// ── #995 follow-up — trusted-hop coverage ───────────────────────────
//
// The original PR only tested the untrusted hop (no ConnectInfo → the
// strip middleware fires and erases the header). Codex pointed out
// that Layer-2 (`decide_resolved_profile_id` + `is_authorized_for_profile`)
// was untested end-to-end because the admin-token harness short-circuits
// authorization in `is_authorized_for_profile`. These tests fix that
// by attaching a `ConnectInfo` (loopback or external) and using a
// non-admin authenticated identity.

/// **TRUSTED + PRESERVE.** A request originating from `127.0.0.1`
/// with a matching `X-Profile-Id` passes the strip middleware
/// unchanged. The auth middleware's proxy-auth branch (header-as-auth)
/// then accepts it and the request resolves to that profile. No 401,
/// no 403 — this is the production Caddy-on-loopback flow.
#[tokio::test]
async fn loopback_request_preserves_x_profile_id_and_authorizes_as_proxy() {
    let dir = TempDir::new().unwrap();
    let state = build_state(&dir, &[("alice", None)]);
    let app = build_router(state).into_service();

    // No `Authorization` header — the auth middleware will fall through
    // to the proxy-auth branch which accepts the `X-Profile-Id` because
    // the hop is trusted. The handler will hit a 503 (no
    // process_manager) which is fine; the contract here is "no 401 / no
    // 403", i.e. the strip middleware did NOT strip and the proxy-auth
    // branch did accept.
    let req = with_connect_info(
        Request::builder()
            .method("GET")
            .uri("/api/files/list")
            .header("x-profile-id", "alice")
            .body(Body::empty())
            .unwrap(),
        loopback_socket_addr(),
    );

    let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();

    assert_ne!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "loopback hop must not strip the legitimate X-Profile-Id; \
         expected proxy-auth to accept and the handler to run"
    );
    assert_ne!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "no identity is even authenticated yet, so 403 would be wrong; \
         got {}",
        resp.status()
    );
}

/// **TRUSTED + AUTHENTICATED USER + CROSS-TENANT HEADER.** This is the
/// exact bypass shape from #995, exercised through the full router
/// (Layer 1 PASS + Layer 2 must DENY). A real OTP-issued session for
/// `alice` attaches `X-Profile-Id: bob` on a loopback hop. The strip
/// middleware preserves the header (trusted hop), the auth middleware
/// promotes the bearer to `AuthIdentity::User { id: "alice", role: User }`,
/// the handler's `decide_resolved_profile_id` (or
/// `authorized_routed_profile_id_from_headers`) sees the cross-tenant
/// header, calls `is_authorized_for_profile`, and returns `403`.
///
/// The pre-fix code path returned the victim's data dir contents
/// silently. The non-admin identity is critical here — the admin
/// short-circuit in `is_authorized_for_profile` would have masked the
/// bug.
#[tokio::test]
async fn authenticated_non_admin_with_cross_tenant_header_on_trusted_hop_is_403() {
    let dir = TempDir::new().unwrap();
    let (state, auth_manager) = build_state_with_users(
        &dir,
        &[
            (
                "alice",
                "alice@example.com",
                octos_cli::user_store::UserRole::User,
            ),
            (
                "bob",
                "bob@example.com",
                octos_cli::user_store::UserRole::User,
            ),
        ],
        &[("alice", None), ("bob", None)],
    );
    let token = mint_session_token(&auth_manager, "alice@example.com").await;
    let app = build_router(state).into_service();

    // Target a route that authorizes the routed profile via
    // `resolve_api_port_authorized` (cancel_task uses that gate first,
    // independent of whether a `process_manager` is wired).
    let req = with_connect_info(
        Request::builder()
            .method("POST")
            .uri("/api/tasks/some-task-id/cancel")
            .header("authorization", format!("Bearer {token}"))
            .header("x-profile-id", "bob")
            .body(Body::empty())
            .unwrap(),
        loopback_socket_addr(),
    );

    let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "authenticated non-admin user with X-Profile-Id pointing to \
         a different tenant on a TRUSTED hop must be 403 — this is the \
         exact #995 bypass shape"
    );
}

/// **TRUSTED + ADMIN + CROSS-TENANT HEADER.** Admin tokens are
/// legitimately allowed to narrow scope via `X-Profile-Id` on a
/// loopback hop — that's how the operator's Caddy ingress
/// authenticates the per-tenant subdomain. The admin
/// short-circuit in `is_authorized_for_profile` returns `true`, so
/// the request proceeds past Layer 2 authorization. The handler then
/// hits a 503 because no `process_manager` is wired in the test
/// fixture, but the contract here is "must NOT be 403" — admin is
/// always allowed.
#[tokio::test]
async fn admin_with_cross_tenant_header_on_trusted_hop_is_not_403() {
    let dir = TempDir::new().unwrap();
    let state = build_state(&dir, &[("alice", None), ("bob", None)]);
    let app = build_router(state).into_service();

    // Target a route that authorizes the routed profile via
    // `resolve_api_port_authorized` so the admin short-circuit in
    // `is_authorized_for_profile` is what's under test (admin must
    // PASS the gate, not be denied).
    let req = with_connect_info(
        Request::builder()
            .method("POST")
            .uri("/api/tasks/some-task-id/cancel")
            .header("authorization", "Bearer admin-secret")
            .header("x-profile-id", "bob")
            .body(Body::empty())
            .unwrap(),
        loopback_socket_addr(),
    );

    let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();

    assert_ne!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "admin auth + cross-tenant header on TRUSTED hop must NOT be \
         403 — this is the legitimate per-tenant narrowing flow that \
         Caddy uses. Got {}",
        resp.status()
    );
    assert_ne!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "admin auth must be honored, not rejected; got {}",
        resp.status()
    );
}

/// **EXTERNAL HOP + AUTHENTICATED USER + CROSS-TENANT HEADER.** When
/// the request comes from a non-trusted address (e.g. an attacker who
/// somehow reached the daemon directly), the Layer-1 strip middleware
/// removes the header BEFORE the auth middleware sees it. The auth
/// middleware then promotes the bearer to alice's identity, no
/// `X-Profile-Id` is left for the handler to honor, and the request
/// resolves to alice's own profile — never bob's. This exercises both
/// Layer 1 (strip on untrusted hop) and Layer 2 (authorization)
/// passing.
#[tokio::test]
async fn authenticated_non_admin_with_cross_tenant_header_on_external_hop_is_stripped() {
    let dir = TempDir::new().unwrap();
    let (state, auth_manager) = build_state_with_users(
        &dir,
        &[
            (
                "alice",
                "alice@example.com",
                octos_cli::user_store::UserRole::User,
            ),
            (
                "bob",
                "bob@example.com",
                octos_cli::user_store::UserRole::User,
            ),
        ],
        &[("alice", None), ("bob", None)],
    );
    let token = mint_session_token(&auth_manager, "alice@example.com").await;
    let app = build_router(state).into_service();

    let req = with_connect_info(
        Request::builder()
            .method("POST")
            .uri("/api/tasks/some-task-id/cancel")
            .header("authorization", format!("Bearer {token}"))
            .header("x-profile-id", "bob")
            .body(Body::empty())
            .unwrap(),
        external_socket_addr(),
    );

    let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();

    // The header was stripped on the external hop, so the handler
    // resolves to alice's own profile (the identity path), not bob.
    // The standalone path (no `process_manager`) returns 503 from the
    // task supervisor; the contract here is the negative one: must
    // NOT be 403 (no cross-tenant header reached the handler), must
    // NOT succeed (no task with that id exists in this fixture, so
    // even a properly-scoped lookup fails closed).
    assert_ne!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "header was stripped before auth, so there is no cross-tenant \
         header to deny; got {}",
        resp.status()
    );
    assert!(
        !resp.status().is_success(),
        "external hop must never succeed in reading another tenant's \
         data via a forged header; got {}",
        resp.status()
    );
}

/// **WS UPGRADE + TRUSTED + AUTHENTICATED USER + CROSS-TENANT HEADER.**
/// The WS upgrade in `ui_protocol.rs:ws_handler` stashes
/// `routed_profile_id_from_headers(...)` on the connection. On a
/// TRUSTED hop the strip middleware preserves the header, so without
/// the GAP-6 fix the connection would carry alice's identity AND bob's
/// `routed_profile_id` into every downstream RPC. After the fix the
/// upgrade returns 403 at the authorization gate, never completing the
/// websocket handshake.
#[tokio::test]
async fn ws_upgrade_with_cross_tenant_header_on_trusted_hop_is_403() {
    let dir = TempDir::new().unwrap();
    let (state, auth_manager) = build_state_with_users(
        &dir,
        &[
            (
                "alice",
                "alice@example.com",
                octos_cli::user_store::UserRole::User,
            ),
            (
                "bob",
                "bob@example.com",
                octos_cli::user_store::UserRole::User,
            ),
        ],
        &[("alice", None), ("bob", None)],
    );
    let token = mint_session_token(&auth_manager, "alice@example.com").await;
    let app = build_router(state).into_service();

    let req = with_connect_info(
        Request::builder()
            .method("GET")
            .uri("/api/ui-protocol/ws")
            .header("authorization", format!("Bearer {token}"))
            .header("x-profile-id", "bob")
            .header("connection", "Upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .body(Body::empty())
            .unwrap(),
        loopback_socket_addr(),
    );

    let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "WS upgrade with cross-tenant X-Profile-Id on TRUSTED hop must \
         be denied before any frames are exchanged — GAP-6 of #995 \
         follow-up review"
    );
}

/// Marker content stored in tenant B's session — used by the admin
/// happy-path test to confirm the response body actually carries B's
/// data (not an empty page from a cross-tenant 403 silently mapped to
/// `[]`, and not an accidental fall-through to tenant A's history).
const TENANT_B_MARKER: &str = "tenant-b-session-marker-payload";

/// Build an `AppState` wired with a real `SessionManager` + a
/// `profile_store`. Persists a single user message with
/// [`TENANT_B_MARKER`] under tenant B's canonical
/// `<profile>:api:<session_id>` key so `session_messages` can return
/// it when authorized. Returns `(state, session_id)`.
async fn build_state_with_sessions_for_tenant_b(
    dir: &TempDir,
    profiles: &[(&str, Option<&str>)],
    tenant_b_id: &str,
    session_id: &str,
) -> Arc<AppState> {
    let profile_store = Arc::new(octos_cli::profiles::ProfileStore::open(dir.path()).unwrap());
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
        profile_store.save(&profile).unwrap();
    }

    let sessions_dir = dir.path().join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    let manager = octos_bus::SessionManager::open(&sessions_dir).unwrap();
    let sessions = Arc::new(tokio::sync::Mutex::new(manager));

    // Persist tenant B's marker message under the canonical profiled
    // key — the same shape `session_messages` probes first. Use a
    // dedicated scope so the lock drops before we hand the manager to
    // the AppState (the helper holds an exclusive lock during seed).
    {
        let key = octos_core::SessionKey::with_profile(tenant_b_id, "api", session_id);
        let mut sess = sessions.lock().await;
        sess.add_message(&key, octos_core::Message::user(TENANT_B_MARKER))
            .await
            .unwrap();
    }

    Arc::new(AppState {
        profile_store: Some(profile_store),
        sessions: Some(sessions),
        auth_token: Some("admin-secret".into()),
        ..AppState::empty_for_tests()
    })
}

/// **#995 follow-up ROUND 3 — `session_messages` STANDALONE PATH.**
///
/// Codex round-2 review flagged the standalone `session_messages`
/// path (`handlers.rs:306` → `api_profile_id_from_headers` raw) as
/// the last bypass-prone surface in this PR:
///
/// > Passing `identity` only affects fallback breadth; it does not
/// > reject cross-tenant headers. WS upgrade likely mitigates current
/// > external reachability, but the handler remains bypass-prone /
/// > re-exposure fragile.
///
/// This test reproduces the exact bypass shape: authenticated
/// non-admin user `alice` on a TRUSTED (loopback) hop with
/// `X-Profile-Id: bob`. Pre-fix, the handler built candidate
/// `SessionKey`s using bob's profile id (the forged header) and
/// returned bob's messages with `200 OK`. Post-fix, the
/// `authorized_routed_profile_id_from_headers` gate at the top of
/// `session_messages` (and the matching gate inside
/// `standalone_api_session_key_candidates_with_topic` for
/// defense-in-depth) rejects the request with `403` before any
/// candidate is constructed.
#[tokio::test]
async fn should_reject_session_messages_cross_tenant_header_on_trusted_hop() {
    let dir = TempDir::new().unwrap();
    let session_id = "web-tenant-b-secret";
    let state = build_state_with_sessions_for_tenant_b(
        &dir,
        &[("alice", None), ("bob", None)],
        "bob",
        session_id,
    )
    .await;

    // Non-admin user identity for `alice`. `AuthIdentity::User` with
    // `UserRole::User` so `is_authorized_for_profile` does not
    // short-circuit on the admin-role branch.
    let identity = Some(Extension(TestAuthIdentity::User {
        id: "alice".into(),
        role: octos_cli::user_store::UserRole::User,
    }));

    // The strip-middleware Layer 1 isn't relevant here (we're calling
    // the handler directly, which is what the WS dispatcher does
    // post-upgrade with the original headers preserved on a trusted
    // hop). What matters is Layer 2: the handler MUST reject the
    // cross-tenant header even though the request appears to come
    // from loopback / a trusted proxy.
    let mut headers = HeaderMap::new();
    headers.insert("x-profile-id", HeaderValue::from_static("bob"));

    let response = test_session_messages(
        axum::extract::State(state),
        headers,
        identity,
        axum::extract::Path(session_id.to_string()),
        axum::extract::Query(TestSessionMessagesPaginationParams {
            limit: 100,
            offset: 0,
            source: None,
            since_seq: None,
            topic: None,
        }),
    )
    .await;

    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "authenticated non-admin user with X-Profile-Id pointing to a \
         different tenant must be rejected with 403 BEFORE any session \
         candidate is built. Got {} — this is the bypass shape codex \
         round-2 flagged.",
        response.status()
    );

    // Belt-and-suspenders: drain the body and confirm the response is
    // NOT carrying tenant B's marker. A status-only check would miss
    // a future regression where the 403 path accidentally serializes
    // the candidate payload.
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = std::str::from_utf8(&body).unwrap_or("<non-utf8>");
    assert!(
        !body_str.contains(TENANT_B_MARKER),
        "403 response body must NOT contain tenant B's marker; got: {body_str}"
    );
}

/// **#995 follow-up ROUND 3 — admin happy path on standalone
/// `session_messages`.**
///
/// Admin auth + a cross-tenant `X-Profile-Id` on a TRUSTED hop is the
/// legitimate per-tenant narrowing flow Caddy uses (admin token in
/// front of a tenant subdomain). `is_authorized_for_profile`
/// short-circuits to `true` on `AuthIdentity::Admin`, so the request
/// MUST proceed past the new authorization gate and actually return
/// tenant B's messages with `200 OK`.
///
/// Codex round 2 specifically flagged the predecessor test ("only
/// asserts not 403/401"). This test goes further: it asserts
/// `StatusCode::OK` AND that the response body literally contains
/// [`TENANT_B_MARKER`], so a regression that silently substitutes
/// tenant A's data (or returns an empty page) is caught.
#[tokio::test]
async fn should_serve_session_messages_when_admin_with_cross_tenant_header_on_trusted_hop() {
    let dir = TempDir::new().unwrap();
    let session_id = "web-tenant-b-canonical";
    let state = build_state_with_sessions_for_tenant_b(
        &dir,
        &[("alice", None), ("bob", None)],
        "bob",
        session_id,
    )
    .await;

    let identity = Some(Extension(TestAuthIdentity::Admin));

    let mut headers = HeaderMap::new();
    headers.insert("x-profile-id", HeaderValue::from_static("bob"));

    let response = test_session_messages(
        axum::extract::State(state),
        headers,
        identity,
        axum::extract::Path(session_id.to_string()),
        axum::extract::Query(TestSessionMessagesPaginationParams {
            limit: 100,
            offset: 0,
            source: None,
            since_seq: None,
            topic: None,
        }),
    )
    .await;

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = std::str::from_utf8(&body).unwrap_or("<non-utf8>");

    assert_eq!(
        status,
        StatusCode::OK,
        "admin + cross-tenant X-Profile-Id on TRUSTED hop must serve \
         the targeted tenant's messages with 200 OK (legitimate Caddy \
         narrowing flow). Got status {status}, body {body_str}"
    );
    assert!(
        body_str.contains(TENANT_B_MARKER),
        "admin response must contain tenant B's marker `{TENANT_B_MARKER}`; \
         got body: {body_str}"
    );
}

/// **Issue #999 — `session/files.list` GATEWAY-MODE TENANT LEAK.**
///
/// Pre-fix `handlers.rs::session_files` short-circuited to
/// `state.sessions.lock().data_dir()` whenever a `SessionManager` was
/// wired — the gateway/standalone top-level — BEFORE checking the
/// host-routed profile. An authenticated non-admin user on a TRUSTED
/// hop with `X-Profile-Id: <victim>` walked straight past the routing
/// layer into whatever workspaces lived under that data_dir, with
/// **`200 OK`** as the response. The correct fix mirrors
/// `session_messages` (#1002): the
/// `authorized_routed_profile_id_from_headers` gate runs FIRST and
/// rejects a cross-tenant header with `403` regardless of which side
/// of the sessions / no-sessions branch ultimately resolves the
/// `data_dir` below.
///
/// Pre-fix expectation: this test returns `200` (the bypass shape).
/// Post-fix expectation: `403` BEFORE any filesystem walk runs.
#[tokio::test]
async fn should_reject_session_files_list_cross_tenant() {
    let dir = TempDir::new().unwrap();
    let session_id = "web-tenant-b-files";
    // Reuse the tenant-B harness — `state.sessions` is wired and the
    // SessionManager already has tenant B's marker message persisted.
    // The shape under test is `session_files` (workspace filesystem
    // listing) not `session_messages`, but the AppState shape and the
    // bypass surface are identical.
    let state = build_state_with_sessions_for_tenant_b(
        &dir,
        &[("alice", None), ("bob", None)],
        "bob",
        session_id,
    )
    .await;

    // Plant a marker file under the path the pre-fix walker would
    // enumerate. `api_session_workspace_dirs` walks
    // `<data_dir>/users/<encoded_session_key>/workspace/` — by
    // dropping a file there we can prove a pre-fix 200 response
    // would have leaked that file's name, while the post-fix path
    // returns 403 before any filesystem walk runs. The plant is
    // best-effort; the contract under test is `status == 403`
    // regardless of whether the plant path is reachable.
    let bare_key = octos_core::SessionKey::new("api", session_id);
    let encoded = octos_bus::session::encode_path_component(bare_key.base_key());
    let workspace = dir.path().join("users").join(&encoded).join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("tenant-b-leak-marker.txt"), b"leak").unwrap();

    let identity = Some(Extension(TestAuthIdentity::User {
        id: "alice".into(),
        role: octos_cli::user_store::UserRole::User,
    }));

    let mut headers = HeaderMap::new();
    headers.insert("x-profile-id", HeaderValue::from_static("bob"));

    let response = test_session_files(
        axum::extract::State(state),
        headers,
        identity,
        axum::extract::Path(session_id.to_string()),
    )
    .await;

    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "authenticated non-admin user with cross-tenant X-Profile-Id \
         must be 403 BEFORE the handler walks `state.sessions.data_dir()` \
         — issue #999. Pre-fix this returned 200 with the workspace \
         listing under the gateway top-level data dir."
    );

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = std::str::from_utf8(&body).unwrap_or("<non-utf8>");
    assert!(
        !body_str.contains("tenant-b-leak-marker"),
        "403 response body must NOT contain the planted marker filename; \
         got: {body_str}"
    );
}

/// **Issue #999 — `session/workspace.get` GATEWAY-MODE TENANT LEAK.**
///
/// Sibling test to `should_reject_session_files_list_cross_tenant`
/// targeting `handlers.rs::session_workspace_contract`. The pre-fix
/// shape is identical: `state.sessions.is_some()` bypassed
/// host-routing entirely and walked
/// `<gateway top-level data_dir>/users/.../workspace/` for repo
/// contracts. Same Layer-2 gate, same contract: cross-tenant header
/// on a TRUSTED hop is `403`.
#[tokio::test]
async fn should_reject_session_workspace_get_cross_tenant() {
    let dir = TempDir::new().unwrap();
    let session_id = "web-tenant-b-workspace";
    let state = build_state_with_sessions_for_tenant_b(
        &dir,
        &[("alice", None), ("bob", None)],
        "bob",
        session_id,
    )
    .await;

    let identity = Some(Extension(TestAuthIdentity::User {
        id: "alice".into(),
        role: octos_cli::user_store::UserRole::User,
    }));

    let mut headers = HeaderMap::new();
    headers.insert("x-profile-id", HeaderValue::from_static("bob"));

    let response = test_session_workspace_contract(
        axum::extract::State(state),
        headers,
        identity,
        axum::extract::Path(session_id.to_string()),
    )
    .await;

    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "authenticated non-admin user with cross-tenant X-Profile-Id \
         must be 403 BEFORE `session_workspace_contract` walks any \
         repository under `state.sessions.data_dir()` — issue #999. \
         Pre-fix this returned 200 with the workspace-contract \
         statuses from the gateway top-level data dir."
    );
}
