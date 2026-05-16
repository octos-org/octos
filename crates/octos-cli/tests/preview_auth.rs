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

struct Fixture {
    _tempdir: TempDir,
    state: Arc<AppState>,
    session_a_id: String,
    session_b_id: String,
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
    profile_store.save(&profile_a).expect("save profile a");
    profile_store.save(&profile_b).expect("save profile b");

    let data_dir_a = profile_store.resolve_data_dir(&profile_a);
    let data_dir_b = profile_store.resolve_data_dir(&profile_b);

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
    let session_b_id = "site-B-9876543210-fedcba";

    let key_a = SessionKey::with_profile(&profile_a.id, "api", session_a_id);
    let key_b = SessionKey::with_profile(&profile_b.id, "api", session_b_id);
    let encoded_a = octos_bus::session::encode_path_component(key_a.base_key());
    let encoded_b = octos_bus::session::encode_path_component(key_b.base_key());

    let ws_a = data_dir_a
        .join("users")
        .join(&encoded_a)
        .join("workspace")
        .join("sites")
        .join(site_slug);
    let ws_b = data_dir_b
        .join("users")
        .join(&encoded_b)
        .join("workspace")
        .join("sites")
        .join(site_slug);
    seed_built_site(&ws_a, "<<<A-CONTENT>>>");
    seed_built_site(&ws_b, "<<<B-CONTENT>>>");

    // 5. AppState wiring. `process_manager`/`session_cache` are not
    //    needed by the preview handler — it only consults
    //    `profile_store` and the on-disk session workspace.
    let state = Arc::new(AppState {
        profile_store: Some(profile_store.clone()),
        user_store: Some(user_store.clone()),
        auth_manager: Some(auth_manager.clone()),
        ..AppState::empty_for_tests()
    });

    Fixture {
        _tempdir: tempdir,
        state,
        session_a_id: session_a_id.into(),
        session_b_id: session_b_id.into(),
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
