//! Compact real-router security representatives for preview authorization,
//! signed-preview token lifetime/quota, and trusted profile-header routing.
#![cfg(feature = "api")]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use axum::response::Response;
use chrono::Utc;
use octos_cli::api::{AppState, PreviewTokens, build_router};
use octos_cli::otp::{AuthManager, DashboardAuthConfig, SmtpConfig};
use octos_cli::profiles::{ProfileConfig, ProfileStore, UserProfile};
use octos_cli::user_store::{User, UserRole, UserStore};
use octos_core::SessionKey;
use serde_json::Value;
use tempfile::TempDir;
use tower::util::ServiceExt;

const TOKEN_A: &str = "PREVIEW-SECURITY-STATIC-TOKEN-A";
const TOKEN_B: &str = "PREVIEW-SECURITY-STATIC-TOKEN-B";

struct Fixture {
    _tempdir: TempDir,
    state: Arc<AppState>,
    session_a: String,
    session_b: String,
    slug: String,
    token_a: String,
}

fn profile(id: &str, name: &str) -> UserProfile {
    UserProfile {
        id: id.into(),
        name: name.into(),
        public_subdomain: None,
        enabled: true,
        data_dir: None,
        parent_id: None,
        config: ProfileConfig::default(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

fn user(id: &str, email: &str, name: &str) -> User {
    User {
        id: id.into(),
        email: email.into(),
        name: name.into(),
        role: UserRole::User,
        created_at: Utc::now(),
        last_login_at: None,
    }
}

fn seed_built_site(workspace: &std::path::Path, slug: &str, marker: &str) {
    std::fs::create_dir_all(workspace.join("dist")).unwrap();
    let metadata = serde_json::json!({
        "version": 1,
        "command": "/new site astro",
        "preset_key": "astro",
        "template": "astro-site",
        "site_kind": "docs",
        "site_name": "Test Site",
        "description": "security fixture",
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
        workspace.join("mofa-site-session.json"),
        serde_json::to_vec(&metadata).unwrap(),
    )
    .unwrap();
    std::fs::write(
        workspace.join("dist/index.html"),
        format!("<!doctype html><html><body>{marker}</body></html>"),
    )
    .unwrap();
}

async fn build_fixture(ttl: Duration) -> Fixture {
    let tempdir = TempDir::new().unwrap();
    let home = tempdir.path();
    let profiles = Arc::new(ProfileStore::open_unified(home).unwrap());
    let a = profile("tenant-a", "Tenant A");
    let b = profile("tenant-b", "Tenant B");
    profiles.save(&a).unwrap();
    profiles.save(&b).unwrap();

    let users = Arc::new(UserStore::open(home).unwrap());
    let alice = user("tenant-a", "alice@example.test", "Alice");
    users.save(&alice).unwrap();
    users
        .save(&user("tenant-b", "bob@example.test", "Bob"))
        .unwrap();
    let auth = Arc::new(AuthManager::new(
        Some(DashboardAuthConfig {
            smtp: Some(SmtpConfig {
                host: "smtp.invalid".into(),
                port: 465,
                username: "no-reply@invalid".into(),
                password_env: "OCTOS_TEST_NO_SMTP".into(),
                from_address: "no-reply@invalid".into(),
            }),
            session_expiry_hours: 1,
            allow_self_registration: false,
            static_tokens: vec![TOKEN_A.into(), TOKEN_B.into()],
        }),
        users.clone(),
    ));
    let token_a = auth
        .verify_otp_with_registration(&alice.email, TOKEN_A, false)
        .await
        .unwrap()
        .unwrap();
    auth.verify_otp_with_registration("bob@example.test", TOKEN_B, false)
        .await
        .unwrap()
        .unwrap();

    let slug = "test-site".to_string();
    let session_a = "site-A-security-1234567890".to_string();
    let session_b = "site-B-security-1234567890".to_string();
    for (profile, session, marker) in [
        (&a, &session_a, "<<<A-CONTENT>>>"),
        (&b, &session_b, "<<<B-CONTENT>>>"),
    ] {
        let key = SessionKey::with_profile(&profile.id, "api", session);
        let encoded = octos_bus::session::encode_path_component(key.base_key());
        let workspace = profiles
            .resolve_data_dir(profile)
            .join("users")
            .join(encoded)
            .join("workspace/sites")
            .join(&slug);
        seed_built_site(&workspace, &slug, marker);
    }

    Fixture {
        _tempdir: tempdir,
        state: Arc::new(AppState {
            profile_store: Some(profiles),
            user_store: Some(users),
            auth_manager: Some(auth),
            preview_tokens: Arc::new(PreviewTokens::with_ttl(ttl)),
            ..AppState::empty_for_tests()
        }),
        session_a,
        session_b,
        slug,
        token_a,
    }
}

async fn get(
    app: &axum::Router,
    uri: &str,
    bearer: Option<&str>,
    profile: Option<&str>,
    trusted: bool,
) -> Response {
    let mut request = Request::builder().method("GET").uri(uri);
    if let Some(token) = bearer {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    if let Some(profile) = profile {
        request = request.header("x-profile-id", profile);
    }
    let mut request = request.body(Body::empty()).unwrap();
    if trusted {
        request.extensions_mut().insert(ConnectInfo(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            65432,
        )));
    }
    app.clone().oneshot(request).await.unwrap()
}

async fn post_json(app: &axum::Router, uri: &str, bearer: Option<&str>, body: Value) -> Response {
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(token) = bearer {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    app.clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}

async fn sign(fx: &Fixture, app: &axum::Router) -> String {
    let response = post_json(
        app,
        "/api/my/preview/sign",
        Some(&fx.token_a),
        serde_json::json!({
            "profile_id": "tenant-a",
            "session_id": fx.session_a,
            "site_slug": fx.slug,
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 16 * 1024)
        .await
        .unwrap();
    serde_json::from_slice::<Value>(&body).unwrap()["preview_url"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn http_otp_login_bearer_and_invalid_credentials() {
    let fx = build_fixture(Duration::from_secs(600)).await;
    let app = build_router(fx.state.clone());
    let response = post_json(
        &app,
        "/api/auth/verify",
        None,
        serde_json::json!({"email": "alice@example.test", "code": TOKEN_A}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 16 * 1024)
        .await
        .unwrap();
    let token = serde_json::from_slice::<Value>(&body).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        get(&app, "/api/auth/me", Some(&token), None, false)
            .await
            .status(),
        StatusCode::OK
    );
    for bearer in [None, Some("wrong-bearer")] {
        assert_eq!(
            get(&app, "/api/auth/me", bearer, None, false)
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
    }
}

#[tokio::test]
async fn preview_auth_and_profile_header_boundaries_fail_closed() {
    let fx = build_fixture(Duration::from_secs(600)).await;
    let app = build_router(fx.state.clone());
    let own = format!(
        "/api/preview/tenant-a/{}/{}/index.html",
        fx.session_a, fx.slug
    );
    assert_eq!(
        get(&app, &own, Some(&fx.token_a), None, false)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        get(&app, &own, None, None, false).await.status(),
        StatusCode::UNAUTHORIZED
    );

    let other = format!(
        "/api/preview/tenant-b/{}/{}/index.html",
        fx.session_b, fx.slug
    );
    let response = get(&app, &other, Some(&fx.token_a), None, false).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&body).contains("<<<B-CONTENT>>>"));

    let side_door = format!("/api/site-preview/{}/{}/index.html", fx.session_b, fx.slug);
    let response = get(&app, &side_door, Some(&fx.token_a), Some("tenant-b"), true).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&body).contains("<<<B-CONTENT>>>"));
}

#[tokio::test]
async fn preview_signed_unknown_expired_and_revoked_tokens_fail_closed() {
    let fx = build_fixture(Duration::from_secs(600)).await;
    let app = build_router(fx.state.clone());
    assert_eq!(
        get(
            &app,
            "/api/preview-signed/00000000000000000000000000000000/index.html",
            None,
            None,
            false,
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );

    let expired = build_fixture(Duration::from_millis(50)).await;
    let expired_app = build_router(expired.state.clone());
    let url = sign(&expired, &expired_app).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        get(&expired_app, &url, None, None, false).await.status(),
        StatusCode::NOT_FOUND
    );

    let url = sign(&fx, &app).await;
    fx.state
        .auth_manager
        .as_ref()
        .unwrap()
        .revoke_session(&fx.token_a)
        .await;
    assert_eq!(
        get(&app, &url, None, None, false).await.status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn preview_signed_cap_returns_429_with_retry_after() {
    let fx = build_fixture(Duration::from_secs(600)).await;
    let app = build_router(fx.state.clone());
    for _ in 0..PreviewTokens::MAX_PER_BEARER {
        sign(&fx, &app).await;
    }
    let response = post_json(
        &app,
        "/api/my/preview/sign",
        Some(&fx.token_a),
        serde_json::json!({
            "profile_id": "tenant-a",
            "session_id": fx.session_a,
            "site_slug": fx.slug,
        }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()["retry-after"], "60");
}
