//! Issue #994 (P0 sev2 cross-tenant data read): the public preview
//! route `/api/preview/{profile_id}/{session_id}/{site_slug}/*` lived
//! on the unauthenticated router branch and resolved profile + session
//! purely from the URL tuple. Any caller who could guess (or harvest)
//! a tuple could read another tenant's built site.
//!
//! These tests pin the post-fix behaviour:
//!
//! 1. Authenticated user A serving their own preview → 200 with content.
//! 2. Authenticated user A serving user B's preview → 403 (profile
//!    ownership mismatch). This is the test that flips from 200 (with
//!    B's content leaked) → 403 across the fix.
//! 3. Authenticated user A pointing at a session that does not belong
//!    to their profile → 403 (session ownership).
//! 4. Unauthenticated hit on any tuple → 401.
//!
//! Pre-fix verification quote: against the public-router build, test 2
//! returns 200 OK with B's `index.html` payload. The fix moves the
//! route to the authenticated branch + asserts identity ownership.

#![cfg(feature = "api")]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use octos_cli::api::{AppState, build_router};
use octos_cli::otp::{AuthManager, DashboardAuthConfig, SmtpConfig};
use octos_cli::profiles::{ProfileConfig, ProfileStore, UserProfile};
use octos_cli::user_store::{User, UserRole, UserStore};
use octos_core::SessionKey;
use tempfile::TempDir;
use tower::util::ServiceExt;

const STATIC_TOKEN_A: &str = "STATIC-TEST-TOKEN-FOR-USER-A";
const STATIC_TOKEN_B: &str = "STATIC-TEST-TOKEN-FOR-USER-B";
/// Bootstrap admin bearer wired into `AppState.auth_token` so the
/// auth middleware resolves it to `AuthIdentity::Admin`. Used by the
/// codex-follow-up tests 9-11 below to pin the admin cross-tenant
/// design contract (see test comments for the design intent).
const ADMIN_BEARER_TOKEN: &str = "STATIC-TEST-TOKEN-FOR-ADMIN";

struct Fixture {
    _tempdir: TempDir,
    state: Arc<AppState>,
    session_a_id: String,
    session_a_other_id: String,
    session_b_id: String,
    /// A session id seeded ONLY under the `admin` profile (NOT under
    /// `tenant-a` or `tenant-b`). When the admin bearer queries this
    /// id without host-routing, the response should resolve to admin's
    /// own data dir — proving `X-Profile-Id` did not cross-tenant.
    session_admin_id: String,
    site_slug: String,
    token_a: String,
    token_b: String,
}

/// Build a fully-wired AppState with two distinct tenant profiles
/// (`tenant-a`, `tenant-b`), corresponding `User` records, a session
/// for each profile pre-seeded with an Astro build output containing
/// the literal markers `<<<A-CONTENT>>>` and `<<<B-CONTENT>>>`.
///
/// Returns the AppState, both profile/session ids, the slug, and the
/// minted session tokens for each user.
async fn build_fixture() -> Fixture {
    let tempdir = TempDir::new().expect("tempdir");
    let octos_home = tempdir.path().to_path_buf();

    // 1. Profile store: two top-level profiles using the default
    //    `<octos_home>/profiles/<id>/data` data-dir layout.
    //    `infer_profile_id_from_data_dir` walks `parent.file_name()`
    //    back up to recover the profile id, so we MUST leave
    //    `data_dir = None` (an override breaks that lookup and the
    //    session-workspace search misses the pre-seeded files).
    let profile_store = Arc::new(ProfileStore::open(&octos_home).expect("profile store"));

    let profile_a = UserProfile {
        id: "tenant-a".into(),
        name: "Tenant A".into(),
        public_subdomain: None,
        enabled: true,
        data_dir: None,
        parent_id: None,
        config: ProfileConfig::default(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let profile_b = UserProfile {
        id: "tenant-b".into(),
        name: "Tenant B".into(),
        public_subdomain: None,
        enabled: true,
        data_dir: None,
        parent_id: None,
        config: ProfileConfig::default(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    // Admin profile. Required so the codex-follow-up tests can
    // exercise the `AuthIdentity::Admin` cross-tenant matrix
    // (header-only vs. host-routed vs. both). Without an `admin`
    // profile row, `resolve_profile_data_dir_by_id(state, "admin")`
    // returns 404 and the test cannot positively assert "admin served
    // their OWN view." See tests 9-11 for the design contract.
    let profile_admin = UserProfile {
        id: "admin".into(),
        name: "Admin".into(),
        public_subdomain: None,
        enabled: true,
        data_dir: None,
        parent_id: None,
        config: ProfileConfig::default(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    profile_store.save(&profile_a).expect("save profile a");
    profile_store.save(&profile_b).expect("save profile b");
    profile_store
        .save(&profile_admin)
        .expect("save profile admin");

    let data_dir_a = profile_store.resolve_data_dir(&profile_a);
    let data_dir_b = profile_store.resolve_data_dir(&profile_b);
    let data_dir_admin = profile_store.resolve_data_dir(&profile_admin);

    // 2. User store: a User per profile, with `id` matching the
    //    profile id so `is_authorized_for_profile` accepts the
    //    identity for its profile.
    let user_store = Arc::new(UserStore::open(&octos_home).expect("user store"));
    let user_a = User {
        id: profile_a.id.clone(),
        email: "alice@example.test".into(),
        name: "Alice".into(),
        role: UserRole::User,
        created_at: Utc::now(),
        last_login_at: None,
    };
    let user_b = User {
        id: profile_b.id.clone(),
        email: "bob@example.test".into(),
        name: "Bob".into(),
        role: UserRole::User,
        created_at: Utc::now(),
        last_login_at: None,
    };
    user_store.save(&user_a).expect("save user a");
    user_store.save(&user_b).expect("save user b");

    // 3. AuthManager configured with static tokens so we can mint a
    //    session token per user without going through the SMTP code
    //    path. `verify_otp_with_registration` accepts the static
    //    token and looks up the user by email — no allow_registration
    //    needed since both users already exist.
    let auth_cfg = DashboardAuthConfig {
        smtp: SmtpConfig {
            host: "smtp.invalid".into(),
            port: 465,
            username: "no-reply@invalid".into(),
            password_env: "OCTOS_TEST_NO_SMTP".into(),
            from_address: "no-reply@invalid".into(),
        },
        session_expiry_hours: 1,
        allow_self_registration: false,
        static_tokens: vec![STATIC_TOKEN_A.into(), STATIC_TOKEN_B.into()],
    };
    let auth_manager = Arc::new(AuthManager::new(Some(auth_cfg), user_store.clone()));

    let token_a = auth_manager
        .verify_otp_with_registration(&user_a.email, STATIC_TOKEN_A, false)
        .await
        .expect("mint token a")
        .expect("token a present");
    let token_b = auth_manager
        .verify_otp_with_registration(&user_b.email, STATIC_TOKEN_B, false)
        .await
        .expect("mint token b")
        .expect("token b present");

    // 4. Pre-seed each profile's session workspace with a minimal
    //    Astro-style project + a built `dist/index.html` so the
    //    preview handler can skip `npm install`/`npm run build`. We
    //    backdate the source mtime so `site_build_needed` returns
    //    false.
    let site_slug = "test-site";
    let session_a_id = "site-A-1234567890-abcdef";
    let session_a_other_id = "site-A-OTHER-7766554433";
    let session_b_id = "site-B-9876543210-fedcba";
    // Admin-owned session, used by the codex-follow-up tests 9-11 to
    // positively assert "admin served their own view, NOT tenant B's"
    // when only `X-Profile-Id` (and no host routing) is supplied.
    let session_admin_id = "site-ADMIN-1111111111-aaaaaa";

    let key_a = SessionKey::with_profile(&profile_a.id, "api", session_a_id);
    let key_a_other = SessionKey::with_profile(&profile_a.id, "api", session_a_other_id);
    let key_b = SessionKey::with_profile(&profile_b.id, "api", session_b_id);
    let key_admin = SessionKey::with_profile(&profile_admin.id, "api", session_admin_id);
    let encoded_a = octos_bus::session::encode_path_component(key_a.base_key());
    let encoded_a_other = octos_bus::session::encode_path_component(key_a_other.base_key());
    let encoded_b = octos_bus::session::encode_path_component(key_b.base_key());
    let encoded_admin = octos_bus::session::encode_path_component(key_admin.base_key());

    let ws_a = data_dir_a
        .join("users")
        .join(&encoded_a)
        .join("workspace")
        .join("sites")
        .join(site_slug);
    let ws_a_other = data_dir_a
        .join("users")
        .join(&encoded_a_other)
        .join("workspace")
        .join("sites")
        .join(site_slug);
    let ws_b = data_dir_b
        .join("users")
        .join(&encoded_b)
        .join("workspace")
        .join("sites")
        .join(site_slug);
    let ws_admin = data_dir_admin
        .join("users")
        .join(&encoded_admin)
        .join("workspace")
        .join("sites")
        .join(site_slug);
    seed_built_site(&ws_a, "<<<A-CONTENT>>>");
    seed_built_site(&ws_a_other, "<<<A-OTHER-CONTENT>>>");
    seed_built_site(&ws_b, "<<<B-CONTENT>>>");
    seed_built_site(&ws_admin, "<<<ADMIN-CONTENT>>>");

    // 5. AppState wiring. `process_manager`/`session_cache` are not
    //    needed by the preview handler — it only consults
    //    `profile_store` and the on-disk session workspace.
    //
    //    `auth_token = Some(ADMIN_BEARER_TOKEN)` lights up the
    //    bootstrap-admin path in `resolve_identity` (no rotated
    //    `admin_token.json` exists for the empty test store, so the
    //    bootstrap token is authoritative). Required by the codex
    //    follow-up tests 9-11 which assert admin cross-tenant
    //    semantics on `/api/site-preview/*`.
    let state = Arc::new(AppState {
        profile_store: Some(profile_store.clone()),
        user_store: Some(user_store.clone()),
        auth_manager: Some(auth_manager.clone()),
        auth_token: Some(ADMIN_BEARER_TOKEN.into()),
        ..AppState::empty_for_tests()
    });

    Fixture {
        _tempdir: tempdir,
        state,
        session_a_id: session_a_id.into(),
        session_a_other_id: session_a_other_id.into(),
        session_b_id: session_b_id.into(),
        session_admin_id: session_admin_id.into(),
        site_slug: site_slug.into(),
        token_a,
        token_b,
    }
}

/// Write `mofa-site-session.json` + `dist/index.html` under `ws_dir`
/// and backdate the source-tree mtime so `site_build_needed` is false
/// (the preview handler skips its npm/quarto build and serves the
/// pre-seeded output directly).
fn seed_built_site(ws_dir: &std::path::Path, marker: &str) {
    use std::time::Duration;

    std::fs::create_dir_all(ws_dir).expect("create site workspace");
    std::fs::create_dir_all(ws_dir.join("dist")).expect("create dist");

    let slug = ws_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("test-site");
    let metadata = serde_json::json!({
        "version": 1,
        "command": "/new site astro",
        "preset_key": "astro",
        "template": "astro-site",
        "site_kind": "docs",
        "site_name": "Test Site",
        "description": "Test fixture",
        "accent": "#000000",
        "reference": "/tmp",
        "reference_label": "tmp",
        "site_slug": slug,
        "preview_base_path": format!("/api/preview/p/s/{slug}"),
        "preview_url": format!("/api/preview/p/s/{slug}/index.html"),
        "build_output_dir": "dist",
        "project_dir": format!("sites/{slug}"),
        "pages": [],
    });
    std::fs::write(
        ws_dir.join("mofa-site-session.json"),
        serde_json::to_vec(&metadata).unwrap(),
    )
    .expect("write metadata");

    let html = format!("<!doctype html><html><body>{marker}</body></html>");
    std::fs::write(ws_dir.join("dist").join("index.html"), html).expect("write index.html");

    // Backdate every source file by ten minutes so `newest_tree_mtime`
    // for the project (excluding `dist`) is older than the `dist`
    // tree's mtime and the preview handler skips the build step.
    let source_mtime = std::time::SystemTime::now() - Duration::from_secs(600);
    if let Ok(file) = std::fs::OpenOptions::new()
        .write(true)
        .open(ws_dir.join("mofa-site-session.json"))
    {
        let _ = file.set_modified(source_mtime);
    }
}

#[tokio::test]
async fn test_1_authed_user_a_serves_own_preview() {
    let fx = build_fixture().await;
    let app = build_router(fx.state.clone());

    let uri = format!(
        "/api/preview/tenant-a/{}/{}/index.html",
        fx.session_a_id, fx.site_slug
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&uri)
                .header("authorization", format!("Bearer {}", fx.token_a))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "user A authenticated against their own profile + session MUST receive 200"
    );
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("<<<A-CONTENT>>>"),
        "expected user A's preview body to contain '<<<A-CONTENT>>>', got: {body_str}"
    );
}

#[tokio::test]
async fn test_2_authed_user_a_cannot_read_user_b_preview() {
    // CROSS-TENANT BLOCK. This is the issue #994 scenario. Before
    // the fix, the route was unauthenticated and resolved profile_id
    // directly from the URL — so user A (or any unauthenticated
    // caller who could guess the tuple) read tenant B's
    // `index.html` with `<<<B-CONTENT>>>`. Post-fix, the
    // authenticated identity must match the route's profile_id.
    let fx = build_fixture().await;
    let app = build_router(fx.state.clone());

    let uri = format!(
        "/api/preview/tenant-b/{}/{}/index.html",
        fx.session_b_id, fx.site_slug
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&uri)
                .header("authorization", format!("Bearer {}", fx.token_a))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "user A authenticated against tenant B's profile MUST be 403, not 200/404; \
         got status {} (this is the issue #994 cross-tenant leak)",
        resp.status()
    );

    // Defence in depth: even if the status check above misfired, the
    // body must NOT contain B's marker.
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        !body_str.contains("<<<B-CONTENT>>>"),
        "cross-tenant body leak: user A's response contains tenant B's marker"
    );
}

#[tokio::test]
async fn test_3_authed_user_a_session_ownership_enforced() {
    // Even within user A's own profile, a session_id that does not
    // belong to A (e.g. crafted / harvested from logs) must not
    // return content. The handler returns 403 so the response is
    // indistinguishable from the cross-tenant case.
    let fx = build_fixture().await;
    let app = build_router(fx.state.clone());

    // user A authenticates, route targets A's profile, but the
    // session id does not match any workspace under A's data dir.
    let uri = format!(
        "/api/preview/tenant-a/site-NOT-OWNED-by-a/{}/index.html",
        fx.site_slug
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&uri)
                .header("authorization", format!("Bearer {}", fx.token_a))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "user A authenticated against an unknown session within their own profile \
         MUST be 403 (session ownership); got status {}",
        resp.status()
    );
}

#[tokio::test]
async fn test_4_unauthenticated_request_rejected() {
    let fx = build_fixture().await;
    let app = build_router(fx.state.clone());

    let uri = format!(
        "/api/preview/tenant-a/{}/{}/index.html",
        fx.session_a_id, fx.site_slug
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "unauthenticated preview request MUST be 401; got status {}",
        resp.status()
    );
}

#[tokio::test]
async fn test_5_authed_user_b_serves_own_preview() {
    // Symmetric to test 1 — ensure auth-handling does not silently
    // alias every authenticated request to one tenant. User B with
    // their own token + own session id must succeed.
    let fx = build_fixture().await;
    let app = build_router(fx.state.clone());

    let uri = format!(
        "/api/preview/tenant-b/{}/{}/index.html",
        fx.session_b_id, fx.site_slug
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&uri)
                .header("authorization", format!("Bearer {}", fx.token_b))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.contains("<<<B-CONTENT>>>"));
}

#[tokio::test]
async fn test_6_reject_site_preview_with_forged_x_profile_id() {
    // Codex review of PR #1001 caught this: `/api/site-preview/*` is a
    // PARALLEL preview surface to `/api/preview/*` (which this PR
    // hardened). Both endpoints can serve the same on-disk content,
    // but `/api/site-preview/*` lacks a `profile_id` URL segment and
    // historically derived the profile via `resolve_profile_data_dir`,
    // which reads the `X-Profile-Id` header. An authenticated tenant A
    // who spoofs `X-Profile-Id: tenant-b` could therefore read
    // tenant B's preview through this side door even after PR #1001
    // closed `/api/preview/*`.
    //
    // Post-fix the handler MUST ignore the header for profile-routing
    // and assert ownership via the authenticated identity (mirroring
    // `serve_owned_site_preview`). Cross-tenant header => 403.
    let fx = build_fixture().await;
    let app = build_router(fx.state.clone());

    // User A authenticates with their own bearer token, but spoofs
    // `X-Profile-Id: tenant-b` AND points at B's session. Pre-fix the
    // handler resolved tenant B's data dir from the header, found B's
    // workspace, and returned `<<<B-CONTENT>>>`.
    let uri = format!(
        "/api/site-preview/{}/{}/index.html",
        fx.session_b_id, fx.site_slug
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&uri)
                .header("authorization", format!("Bearer {}", fx.token_a))
                .header("x-profile-id", "tenant-b")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "user A spoofing X-Profile-Id: tenant-b MUST be 403, not 200 with B's data; \
         got status {} (this is the codex-flagged /api/site-preview/* side door)",
        resp.status()
    );

    // Defence in depth: even if status check misfired, B's marker
    // must not leak.
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        !body_str.contains("<<<B-CONTENT>>>"),
        "cross-tenant body leak via /api/site-preview/* with forged X-Profile-Id: \
         response contains tenant B's marker"
    );
}

#[tokio::test]
async fn test_7_serve_site_preview_with_authenticated_identity() {
    // Codex review companion to test 6. With `/api/site-preview/*` no
    // longer trusting `X-Profile-Id`, an authenticated user A hitting
    // their OWN session and no `X-Profile-Id` header must still get
    // 200 — the handler derives the profile from the authenticated
    // identity instead.
    let fx = build_fixture().await;
    let app = build_router(fx.state.clone());

    let uri = format!(
        "/api/site-preview/{}/{}/index.html",
        fx.session_a_id, fx.site_slug
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&uri)
                .header("authorization", format!("Bearer {}", fx.token_a))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "user A authenticated against their own session without X-Profile-Id MUST be 200; \
         got status {}",
        resp.status()
    );
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("<<<A-CONTENT>>>"),
        "expected user A's preview body to contain '<<<A-CONTENT>>>', got: {body_str}"
    );
}

#[tokio::test]
async fn test_8_serve_other_session_under_same_profile() {
    // Codex review §3 "session-ownership semantics" clarification.
    //
    // `/api/site-preview/*` is intentionally profile-scoped, NOT
    // per-session-owner: any session that lives under the
    // authenticated identity's profile is reachable, even one the
    // user didn't directly originate from this browser. This is the
    // current product semantic — multiple browsers / devices for the
    // same tenant should all be able to load the tenant's previews.
    //
    // Test 3 above already pinned "session that does NOT exist under
    // the profile -> 403". This test pins the complementary case:
    // session that DOES exist under user A's profile (different
    // `web-<id>` than the user's "current" session) -> 200. If the
    // product later switches to per-session-owner semantics, this
    // test should flip and capture the intent change.
    let fx = build_fixture().await;
    let app = build_router(fx.state.clone());

    // session_a_other_id is pre-seeded under tenant-a's data dir but
    // is a DIFFERENT id from `session_a_id`. With token A and no
    // X-Profile-Id, the handler must resolve tenant-a's data dir from
    // the identity and serve the other session under the same profile.
    let uri = format!(
        "/api/site-preview/{}/{}/index.html",
        fx.session_a_other_id, fx.site_slug
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&uri)
                .header("authorization", format!("Bearer {}", fx.token_a))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "user A hitting another of A's profile-owned sessions via /api/site-preview/* \
         MUST be 200 (profile-scoped semantic); got status {}",
        resp.status()
    );
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("<<<A-OTHER-CONTENT>>>"),
        "expected the other A-session's marker, got: {body_str}"
    );
}

// ---------------------------------------------------------------
// Codex round-2 re-review follow-ups (admin cross-tenant matrix).
//
// Codex flagged: "Admin + `X-Profile-Id: tenant-b` is allowed past the
// spoof check, but the header is not used for routing; it falls
// through to `ADMIN_PROFILE_ID`, so it will not serve tenant B unless
// host-routing selected B earlier."
//
// This is the current behavior and an INTENTIONAL design choice (not
// a bug). Tests 9-11 below pin the three corners of the matrix so the
// contract is enforced by CI:
//
// | host routing | X-Profile-Id | result                          |
// |--------------|--------------|---------------------------------|
// | no           | tenant-b     | serves admin's OWN view (T9)    |
// | tenant-b     | (absent)     | serves tenant B (T10)           |
// | tenant-b     | tenant-b     | serves tenant B (T11 sanity)    |
//
// Design intent: admin cross-tenant access flows ONLY through
// host-routing (audit-friendly: each tenant has its own subdomain in
// the access log). `X-Profile-Id` is NOT honored as a cross-tenant
// switch for admin — that path would be too easily masqueraded by
// misbehaving tooling or an SSRF that controls headers but not host.
// ---------------------------------------------------------------

#[tokio::test]
async fn test_9_admin_with_forged_x_profile_id_does_not_cross_tenant_via_header() {
    // Design intent: header alone is not sufficient for cross-tenant
    // admin access; host routing is required.
    //
    // Admin authenticates with the bootstrap admin bearer and sends
    // `X-Profile-Id: tenant-b`, with NO host-routing (the default
    // tower-oneshot request has no `Host` header → `request_host`
    // returns `None`). The route then falls through to
    // `identity_profile_id = ADMIN_PROFILE_ID`, resolving admin's own
    // data dir. The defense-in-depth spoof check (handlers.rs:2146)
    // does not 403 here because `is_authorized_for_profile(Admin, *)`
    // is unconditionally true — but the header is still ignored for
    // routing.
    //
    // We request `session_admin_id` (seeded ONLY under admin's data
    // dir). If the header were honored we'd resolve tenant-b's dir,
    // not find the admin session, and 404. The fact that we serve
    // 200 with `<<<ADMIN-CONTENT>>>` proves the header did NOT switch
    // the routed tenant.
    let fx = build_fixture().await;
    let app = build_router(fx.state.clone());

    let uri = format!(
        "/api/site-preview/{}/{}/index.html",
        fx.session_admin_id, fx.site_slug
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&uri)
                .header("authorization", format!("Bearer {ADMIN_BEARER_TOKEN}"))
                .header("x-profile-id", "tenant-b")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "admin + X-Profile-Id: tenant-b (no host routing) MUST resolve to admin's \
         own data dir and serve 200 for an admin-owned session; got status {}. \
         If this flips to 404, X-Profile-Id likely started routing the request \
         and that is the regression this test guards against.",
        resp.status()
    );
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("<<<ADMIN-CONTENT>>>"),
        "expected admin's own marker — header must NOT switch tenants; got: {body_str}"
    );
    assert!(
        !body_str.contains("<<<B-CONTENT>>>"),
        "cross-tenant body leak: admin + forged X-Profile-Id returned tenant B's marker"
    );
}

#[tokio::test]
async fn test_10_admin_host_routed_does_cross_tenant() {
    // Design intent: admin cross-tenant access uses host routing.
    //
    // Admin authenticates with the bootstrap admin bearer. The Host
    // header is set to a subdomain that resolves to `tenant-b` via
    // `ProfileStore::resolve_routable_profile_id`. No `X-Profile-Id`
    // is sent — host alone is the cross-tenant switch.
    //
    // The handler resolves the subdomain candidate `tenant-b` against
    // the profile store, finds the top-level profile (no
    // `public_subdomain`, no `parent_id` ⇒ the immutable internal id
    // is routable), checks `is_authorized_for_profile(Admin, "tenant-b")
    // = true`, and serves tenant B's data dir. The request URL points
    // at B's session id (`session_b_id`), which exists under B's data
    // dir, so the handler returns B's seeded marker.
    let fx = build_fixture().await;
    let app = build_router(fx.state.clone());

    let uri = format!(
        "/api/site-preview/{}/{}/index.html",
        fx.session_b_id, fx.site_slug
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&uri)
                .header("authorization", format!("Bearer {ADMIN_BEARER_TOKEN}"))
                // `request_host` reads `host`/`x-forwarded-host`,
                // strips the port, lowercases, and takes the first
                // dot-segment as the candidate. `tenant-b` resolves
                // through `resolve_routable_profile_id` because the
                // tenant-b profile is top-level with no
                // `public_subdomain` set.
                .header("host", "tenant-b.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "admin + host-routed to tenant-b (no X-Profile-Id) MUST serve tenant B's \
         data dir for an existing tenant-b session; got status {}",
        resp.status()
    );
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("<<<B-CONTENT>>>"),
        "expected tenant B's marker via host routing, got: {body_str}"
    );
}

#[tokio::test]
async fn test_11_admin_with_host_and_matching_x_profile_id() {
    // Sanity check: when the consistent host AND `X-Profile-Id` both
    // point at the same tenant, the request still serves that tenant
    // (i.e. the defense-in-depth spoof check at handlers.rs:2146 does
    // not regress and reject a consistent header). Host routing is
    // authoritative; a matching header is a no-op.
    let fx = build_fixture().await;
    let app = build_router(fx.state.clone());

    let uri = format!(
        "/api/site-preview/{}/{}/index.html",
        fx.session_b_id, fx.site_slug
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&uri)
                .header("authorization", format!("Bearer {ADMIN_BEARER_TOKEN}"))
                .header("host", "tenant-b.example.com")
                .header("x-profile-id", "tenant-b")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "admin + host=tenant-b + X-Profile-Id=tenant-b (consistent) MUST serve \
         tenant B; got status {}",
        resp.status()
    );
    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("<<<B-CONTENT>>>"),
        "expected tenant B's marker, got: {body_str}"
    );
}
