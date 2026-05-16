//! Issue #1001 follow-up: PR #1001 closed the cross-tenant
//! `/api/preview/{profile_id}/...` leak by requiring auth on every
//! request. That regressed the SPA iframe UX — `<iframe src=...>`
//! cannot send `Authorization: Bearer ...`, so the preview iframe
//! 401-loops after the dashboard tab loads.
//!
//! Codex design: mint an opaque 256-bit random token via
//! `POST /api/my/preview/sign`, stash a server-side grant
//! `{issuer_bearer, identity, profile_id, session_id, site_slug,
//! expires_at}` in a process-local cache, and serve preview content
//! through the public route `GET /api/preview-signed/{token}/{*path}`.
//! The token is in the PATH (not a query param) so relative assets
//! under the preview HTML inherit the prefix without rewriting.
//!
//! This test suite pins:
//!  - 1: authenticated sign for own profile → 200 with token + url + expires_at
//!  - 2: authenticated sign for other tenant → 403
//!  - 3: unauthenticated sign → 401
//!  - 4: GET signed-preview/{valid_token}/index.html → 200 with `Referrer-Policy: no-referrer`
//!  - 5: GET signed-preview/{invalid_token}/index.html → 404 (don't leak token existence)
//!  - 6: GET signed-preview after expiry → 404 (short test TTL)
//!  - 7: GET signed-preview after issuer bearer revoked → 403
//!  - 8: path traversal in `{*path}` → 404 (relies on PR #1000's symlink-safe walk)
//!  - 9: per-bearer cap reached → 429 (codex GAP 8: DoS protection)
//!  - 10: background sweeper removes expired tokens (codex NEEDS-FOLLOWUP 6:
//!    idle daemons must not accumulate expired entries indefinitely)

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
use serde_json::Value;
use tempfile::TempDir;
use tower::util::ServiceExt;

const STATIC_TOKEN_A: &str = "PREVIEW-SIGNED-STATIC-TOKEN-A";
const STATIC_TOKEN_B: &str = "PREVIEW-SIGNED-STATIC-TOKEN-B";

struct Fixture {
    _tempdir: TempDir,
    state: Arc<AppState>,
    session_a_id: String,
    site_slug: String,
    token_a: String,
    /// Held in the fixture so the user-B session in `AuthManager` exists
    /// at test time even though the current suite only exercises user A
    /// flows. Keeps the fixture symmetric with `preview_auth.rs` so
    /// future cross-tenant tests (e.g. "user A's signed token cannot
    /// serve user B's content") can reuse the harness without
    /// retrofitting user B.
    #[allow(dead_code)]
    token_b: String,
}

async fn build_fixture() -> Fixture {
    build_fixture_with_ttl(std::time::Duration::from_secs(600)).await
}

async fn build_fixture_with_ttl(ttl: std::time::Duration) -> Fixture {
    let tempdir = TempDir::new().expect("tempdir");
    let octos_home = tempdir.path().to_path_buf();

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
    profile_store.save(&profile_a).expect("save a");
    profile_store.save(&profile_b).expect("save b");

    let data_dir_a = profile_store.resolve_data_dir(&profile_a);

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
        .expect("token a");
    let token_b = auth_manager
        .verify_otp_with_registration(&user_b.email, STATIC_TOKEN_B, false)
        .await
        .expect("mint token b")
        .expect("token b");

    let site_slug = "test-site";
    let session_a_id = "site-A-signed-1234567890";

    let key_a = SessionKey::with_profile(&profile_a.id, "api", session_a_id);
    let encoded_a = octos_bus::session::encode_path_component(key_a.base_key());
    let ws_a = data_dir_a
        .join("users")
        .join(&encoded_a)
        .join("workspace")
        .join("sites")
        .join(site_slug);
    seed_built_site(&ws_a, "<<<A-CONTENT>>>");

    let state = Arc::new(AppState {
        profile_store: Some(profile_store.clone()),
        user_store: Some(user_store.clone()),
        auth_manager: Some(auth_manager.clone()),
        preview_tokens: Arc::new(octos_cli::api::PreviewTokens::with_ttl(ttl)),
        ..AppState::empty_for_tests()
    });

    Fixture {
        _tempdir: tempdir,
        state,
        session_a_id: session_a_id.into(),
        site_slug: site_slug.into(),
        token_a,
        token_b,
    }
}

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

    let source_mtime = std::time::SystemTime::now() - Duration::from_secs(600);
    if let Ok(file) = std::fs::OpenOptions::new()
        .write(true)
        .open(ws_dir.join("mofa-site-session.json"))
    {
        let _ = file.set_modified(source_mtime);
    }
}

async fn sign_preview_request(
    app: axum::Router,
    bearer: Option<&str>,
    profile_id: &str,
    session_id: &str,
    site_slug: &str,
) -> axum::response::Response {
    let body = serde_json::json!({
        "profile_id": profile_id,
        "session_id": session_id,
        "site_slug": site_slug,
    })
    .to_string();
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/my/preview/sign")
        .header("content-type", "application/json");
    if let Some(token) = bearer {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    app.oneshot(req.body(Body::from(body)).unwrap())
        .await
        .unwrap()
}

// ---- Tests ----------------------------------------------------------

#[tokio::test]
async fn test_1_sign_own_profile_returns_token_and_url() {
    let fx = build_fixture().await;
    let app = build_router(fx.state.clone());

    let resp = sign_preview_request(
        app,
        Some(&fx.token_a),
        "tenant-a",
        &fx.session_a_id,
        &fx.site_slug,
    )
    .await;

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "authenticated sign for own profile must succeed"
    );

    let body = axum::body::to_bytes(resp.into_body(), 16 * 1024)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).expect("sign body should be JSON");
    let token = json
        .get("token")
        .and_then(|v| v.as_str())
        .expect("token field");
    assert!(
        token.len() >= 32,
        "token must be sufficiently random (got len={})",
        token.len()
    );
    let preview_url = json
        .get("preview_url")
        .and_then(|v| v.as_str())
        .expect("preview_url field");
    assert!(
        preview_url.starts_with(&format!("/api/preview-signed/{token}/")),
        "preview_url must embed the token in path; got: {preview_url}"
    );
    assert!(
        preview_url.ends_with("/index.html"),
        "preview_url default leaf is /index.html; got: {preview_url}"
    );
    assert!(
        json.get("expires_at").is_some(),
        "expires_at field must be present"
    );
}

#[tokio::test]
async fn test_2_sign_other_profile_is_forbidden() {
    let fx = build_fixture().await;
    let app = build_router(fx.state.clone());

    // user A's bearer trying to sign a preview URL for tenant B
    let resp = sign_preview_request(
        app,
        Some(&fx.token_a),
        "tenant-b",
        &fx.session_a_id,
        &fx.site_slug,
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "user A signing for tenant-b MUST be 403"
    );
}

#[tokio::test]
async fn test_3_sign_unauthenticated_is_unauthorized() {
    let fx = build_fixture().await;
    let app = build_router(fx.state.clone());

    let resp = sign_preview_request(app, None, "tenant-a", &fx.session_a_id, &fx.site_slug).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "unauthenticated sign POST must be 401"
    );
}

#[tokio::test]
async fn test_4_get_signed_preview_serves_content_with_referrer_policy() {
    let fx = build_fixture().await;
    let app = build_router(fx.state.clone());

    let sign_resp = sign_preview_request(
        app.clone(),
        Some(&fx.token_a),
        "tenant-a",
        &fx.session_a_id,
        &fx.site_slug,
    )
    .await;
    assert_eq!(sign_resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(sign_resp.into_body(), 16 * 1024)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).expect("sign JSON");
    let preview_url = json
        .get("preview_url")
        .and_then(|v| v.as_str())
        .expect("preview_url")
        .to_string();

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&preview_url)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "valid signed-preview token MUST serve content (no Authorization header needed)"
    );

    let referrer_policy = resp
        .headers()
        .get("referrer-policy")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    assert_eq!(
        referrer_policy.as_deref(),
        Some("no-referrer"),
        "signed-preview responses MUST set Referrer-Policy: no-referrer (codex design)"
    );

    let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("<<<A-CONTENT>>>"),
        "expected preview body, got: {body_str}"
    );
}

#[tokio::test]
async fn test_5_invalid_token_returns_404_not_401() {
    let fx = build_fixture().await;
    let app = build_router(fx.state.clone());

    // Bogus token — handler must NOT leak whether it exists.
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/preview-signed/00000000000000000000000000000000/index.html")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "invalid token must be 404 (NOT 401), to not leak token existence"
    );
}

#[tokio::test]
async fn test_6_expired_token_returns_404() {
    // Build a fixture with a tiny TTL so the token is already expired
    // by the time we GET it. Sleep is short to keep CI snappy.
    let fx = build_fixture_with_ttl(std::time::Duration::from_millis(50)).await;
    let app = build_router(fx.state.clone());

    let sign_resp = sign_preview_request(
        app.clone(),
        Some(&fx.token_a),
        "tenant-a",
        &fx.session_a_id,
        &fx.site_slug,
    )
    .await;
    assert_eq!(sign_resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(sign_resp.into_body(), 16 * 1024)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).expect("sign JSON");
    let preview_url = json
        .get("preview_url")
        .and_then(|v| v.as_str())
        .expect("preview_url")
        .to_string();

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&preview_url)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "expired token must be treated like an unknown token (404, no leak)"
    );
}

#[tokio::test]
async fn test_7_revoked_bearer_returns_403() {
    let fx = build_fixture().await;
    let app = build_router(fx.state.clone());

    let sign_resp = sign_preview_request(
        app.clone(),
        Some(&fx.token_a),
        "tenant-a",
        &fx.session_a_id,
        &fx.site_slug,
    )
    .await;
    assert_eq!(sign_resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(sign_resp.into_body(), 16 * 1024)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).expect("sign JSON");
    let preview_url = json
        .get("preview_url")
        .and_then(|v| v.as_str())
        .expect("preview_url")
        .to_string();

    // Revoke user A's bearer via the same AuthManager the fixture used.
    fx.state
        .auth_manager
        .as_ref()
        .expect("auth manager wired")
        .revoke_session(&fx.token_a)
        .await;

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(&preview_url)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "after issuer bearer revoked, signed-preview must 403 (re-validation)"
    );
}

#[tokio::test]
async fn test_8_path_traversal_in_signed_preview_is_blocked() {
    let fx = build_fixture().await;
    let app = build_router(fx.state.clone());

    let sign_resp = sign_preview_request(
        app.clone(),
        Some(&fx.token_a),
        "tenant-a",
        &fx.session_a_id,
        &fx.site_slug,
    )
    .await;
    assert_eq!(sign_resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(sign_resp.into_body(), 16 * 1024)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&body).expect("sign JSON");
    let token = json
        .get("token")
        .and_then(|v| v.as_str())
        .expect("token")
        .to_string();

    // ../../etc/passwd attack via the path component. Relies on the
    // symlink-safe walker from PR #1000 + `resolve_preview_asset_path`
    // refusing parent traversal.
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/preview-signed/{token}/../../../etc/passwd"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        matches!(
            resp.status(),
            StatusCode::NOT_FOUND | StatusCode::BAD_REQUEST
        ),
        "path traversal must be refused (404 or 400), not 200; got: {}",
        resp.status()
    );
}

/// Codex GAP 8 (blocking): an authenticated user can mint unbounded
/// preview tokens, each held 10 min in memory. DoS vector — a hostile
/// (or buggy) client could pump the in-memory map to OOM the daemon.
///
/// Fix: cap concurrent grants per `(issuer_bearer)` at
/// `MAX_PER_BEARER` (64). When the cap is reached, the next mint
/// returns HTTP 429 instead of growing the map.
///
/// This test mints `MAX_PER_BEARER` tokens via the HTTP surface, asserts
/// every one returns 200, then asserts mint number `MAX_PER_BEARER + 1`
/// returns 429. The TTL is long enough that the lazy expiry sweep
/// won't open up a slot mid-test.
#[tokio::test]
async fn test_9_per_bearer_cap_returns_429() {
    let fx = build_fixture().await;
    let app = build_router(fx.state.clone());

    let cap = octos_cli::api::PreviewTokens::MAX_PER_BEARER;

    // Mint up to the cap — every one must succeed.
    for i in 0..cap {
        let resp = sign_preview_request(
            app.clone(),
            Some(&fx.token_a),
            "tenant-a",
            &fx.session_a_id,
            &fx.site_slug,
        )
        .await;
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "mint #{i} (within per-bearer cap of {cap}) must succeed; got {}",
            resp.status()
        );
    }

    // Cap + 1: must be 429 (rate limited).
    let resp = sign_preview_request(
        app.clone(),
        Some(&fx.token_a),
        "tenant-a",
        &fx.session_a_id,
        &fx.site_slug,
    )
    .await;
    assert_eq!(
        resp.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "mint #{cap}+1 must be 429 (per-bearer cap reached); got {}",
        resp.status()
    );
}

/// Codex NEEDS-FOLLOWUP 6: expired tokens are only swept on
/// `issue`/`consume`. An idle daemon (no preview activity for hours)
/// accumulates expired entries indefinitely.
///
/// Fix: `PreviewTokens::spawn_background_sweeper` starts a tokio task
/// that calls `sweep_expired_all` every ~60s in production. The
/// sweeper accepts a configurable interval for tests so we don't have
/// to wait 60s in CI.
///
/// This test:
///   1. Builds a cache with a tiny TTL (50ms).
///   2. Mints a token (cache.len() == 1).
///   3. Spawns the background sweeper with a 20ms interval.
///   4. Waits past TTL + a couple sweep cycles.
///   5. Asserts the cache is empty — the background sweeper removed
///      the expired grant WITHOUT any `issue`/`consume` traffic.
#[tokio::test]
async fn test_10_background_sweeper_removes_expired() {
    use std::time::Duration;

    let cache = std::sync::Arc::new(octos_cli::api::PreviewTokens::with_ttl(
        Duration::from_millis(50),
    ));

    // Seed the cache with one token. We use the in-process API directly
    // (no HTTP) — the background-sweeper contract is on the cache type,
    // not on the routing layer.
    let signed = cache
        .issue(
            "BEARER-A".into(),
            octos_cli::api::TestAuthIdentity::User {
                id: "tenant-a".into(),
                role: octos_cli::user_store::UserRole::User,
            },
            "tenant-a".into(),
            "session-1".into(),
            "site-a".into(),
        )
        .await
        .expect("issue");
    assert_eq!(cache.len().await, 1, "fresh token must be cached");

    // Spawn the background sweeper with a 20ms interval (fast enough to
    // sweep the 50ms-TTL token within the test's 250ms wait window).
    let _handle = octos_cli::api::PreviewTokens::spawn_background_sweeper(
        cache.clone(),
        Duration::from_millis(20),
    );

    // Wait past TTL + several sweep cycles. 250ms = 5x TTL = ~12x
    // sweep interval, well over the minimum to guarantee at least one
    // sweep after the token expires.
    tokio::time::sleep(Duration::from_millis(250)).await;

    assert_eq!(
        cache.len().await,
        0,
        "background sweeper MUST have removed the expired token \
         (was: token {} still present after 250ms with 50ms TTL + 20ms sweep)",
        signed.token
    );
}
