//! API router construction.

use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::routing::{delete, get, post, put};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use super::AppState;
use super::admin;
use super::auth_handlers;
use super::frps_plugin;
use super::handlers;
use super::metrics;
use super::static_files;
use super::user_admin;
use super::webhook_proxy;
use crate::user_store::UserRole;

/// Authentication identity extracted by the auth middleware.
#[derive(Clone, Debug)]
pub enum AuthIdentity {
    /// Admin token — full access to all endpoints.
    Admin,
    /// Authenticated user session.
    User { id: String, role: UserRole },
}

/// Build the axum router with all API routes.
pub fn build_router(state: Arc<AppState>) -> Router {
    // Restrict CORS to an explicit allowlist of known origins.
    // Do NOT use suffix matching (e.g. ends_with(".ominix.io")) — a hijacked
    // subdomain would pass the check and enable cross-origin requests.
    const ALLOWED_ORIGINS: &[&str] = &[
        "https://app.ominix.io",
        "https://admin.ominix.io",
        "https://api.ominix.io",
        "https://app.crew.ominix.io",
        "https://admin.crew.ominix.io",
        "https://api.crew.ominix.io",
        "http://localhost:3000",
        "http://localhost:5173",
    ];
    let cors = CorsLayer::new()
        .allow_origin(tower_http::cors::AllowOrigin::predicate(|origin, _| {
            let o = origin.to_str().unwrap_or("");
            ALLOWED_ORIGINS.contains(&o)
        }))
        .allow_methods(tower_http::cors::Any)
        .allow_headers(tower_http::cors::Any);

    // Public auth endpoints (no auth required)
    let auth_api = Router::new()
        .route("/api/auth/send-code", post(auth_handlers::send_code))
        .route("/api/auth/verify", post(auth_handlers::verify))
        .route("/api/auth/logout", post(auth_handlers::logout));

    // Chat + status API (existing)
    let chat_api = Router::new()
        .route("/api/chat", post(handlers::chat))
        .route("/api/chat/stream", get(handlers::chat_stream))
        .route("/api/ws", get(handlers::ws_handler))
        .route(
            "/api/upload",
            post(handlers::upload).layer(DefaultBodyLimit::max(100 * 1024 * 1024)),
        )
        .route(
            "/api/site-files/upload",
            post(handlers::upload_site_files).layer(DefaultBodyLimit::max(100 * 1024 * 1024)),
        )
        .route(
            "/api/site-preview/{session_id}/{site_slug}",
            get(handlers::serve_site_preview_root),
        )
        .route(
            "/api/site-preview/{session_id}/{site_slug}/",
            get(handlers::serve_site_preview_root),
        )
        .route(
            "/api/site-preview/{session_id}/{site_slug}/{*path}",
            get(handlers::serve_site_preview_path),
        )
        .route("/api/files/list", get(handlers::list_content_files))
        .route("/api/files/{filename}", get(handlers::serve_file))
        .route("/api/files", get(handlers::serve_file_by_query))
        .route("/api/sessions", get(handlers::list_sessions))
        .route(
            "/api/sessions/{id}/messages",
            get(handlers::session_messages),
        )
        .route("/api/sessions/{id}/status", get(handlers::session_status))
        .route("/api/sessions/{id}/tasks", get(handlers::session_tasks))
        .route("/api/sessions/{id}", delete(handlers::delete_session))
        .route("/api/status", get(handlers::status));

    // User self-service endpoints (user or admin auth)
    let my_api = Router::new()
        .route("/api/my/profile", get(auth_handlers::my_profile))
        .route("/api/my/profile", put(auth_handlers::update_my_profile))
        .route("/api/my/soul", get(auth_handlers::my_soul))
        .route("/api/my/soul", put(auth_handlers::update_my_soul))
        .route("/api/my/soul", delete(auth_handlers::delete_my_soul))
        .route("/api/my/content", get(auth_handlers::my_content))
        .route(
            "/api/my/content/{id}/thumbnail",
            get(auth_handlers::my_content_thumbnail),
        )
        .route(
            "/api/my/content/{id}/body",
            get(auth_handlers::my_content_body),
        )
        .route(
            "/api/my/content/{id}",
            delete(auth_handlers::delete_my_content),
        )
        .route(
            "/api/my/content/bulk-delete",
            post(auth_handlers::bulk_delete_my_content),
        )
        .route(
            "/api/my/profile/start",
            post(auth_handlers::start_my_gateway),
        )
        .route("/api/my/profile/stop", post(auth_handlers::stop_my_gateway))
        .route(
            "/api/my/profile/restart",
            post(auth_handlers::restart_my_gateway),
        )
        .route(
            "/api/my/profile/status",
            get(auth_handlers::my_gateway_status),
        )
        .route("/api/my/profile/logs", get(auth_handlers::my_gateway_logs))
        .route(
            "/api/my/profile/whatsapp/qr",
            get(auth_handlers::my_whatsapp_qr),
        )
        .route(
            "/api/my/profile/wechat/qr-start",
            get(auth_handlers::my_wechat_qr_start),
        )
        .route(
            "/api/my/profile/wechat/qr-poll",
            post(auth_handlers::my_wechat_qr_poll),
        )
        .route(
            "/api/my/profile/metrics",
            get(auth_handlers::my_provider_metrics),
        )
        .route("/api/auth/me", get(auth_handlers::me))
        .route("/api/my/test-provider", post(admin::test_provider))
        .route("/api/my/provider-models", post(admin::provider_models))
        .route("/api/my/test-search", post(admin::test_search))
        .route("/api/my/model-limits", get(admin::model_limits))
        .route(
            "/api/my/profile/accounts",
            get(auth_handlers::my_sub_accounts),
        )
        .route(
            "/api/my/profile/accounts/{id}/start",
            post(auth_handlers::start_my_sub_gateway),
        )
        .route(
            "/api/my/profile/accounts/{id}/stop",
            post(auth_handlers::stop_my_sub_gateway),
        )
        // Self-service tenant registration (user-auth level)
        .route("/api/register", post(admin::register_tenant))
        .route(
            "/api/register/setup-script",
            get(admin::register_setup_script),
        );

    // Admin API routes (admin auth only, 1MB body limit)
    let admin_api = Router::new()
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .route("/api/admin/overview", get(admin::overview))
        .route("/api/admin/profiles", get(admin::list_profiles))
        .route("/api/admin/profiles", post(admin::create_profile))
        .route("/api/admin/profiles/{id}", get(admin::get_profile))
        .route("/api/admin/profiles/{id}", put(admin::update_profile))
        .route("/api/admin/profiles/{id}", delete(admin::delete_profile))
        .route("/api/admin/profiles/{id}/start", post(admin::start_gateway))
        .route("/api/admin/profiles/{id}/stop", post(admin::stop_gateway))
        .route(
            "/api/admin/profiles/{id}/restart",
            post(admin::restart_gateway),
        )
        .route(
            "/api/admin/profiles/{id}/status",
            get(admin::gateway_status),
        )
        .route("/api/admin/profiles/{id}/logs", get(admin::gateway_logs))
        .route(
            "/api/admin/profiles/{id}/metrics",
            get(admin::provider_metrics),
        )
        .route(
            "/api/admin/profiles/{id}/whatsapp/qr",
            get(admin::whatsapp_qr),
        )
        .route(
            "/api/admin/profiles/{id}/wechat/qr-start",
            get(admin::wechat_qr_start),
        )
        .route(
            "/api/admin/profiles/{id}/wechat/qr-poll",
            post(admin::wechat_qr_poll),
        )
        .route("/api/admin/test-provider", post(admin::test_provider))
        .route("/api/admin/start-all", post(admin::start_all))
        .route("/api/admin/stop-all", post(admin::stop_all))
        // Sub-account management
        .route(
            "/api/admin/profiles/{id}/accounts",
            get(admin::list_sub_accounts),
        )
        .route(
            "/api/admin/profiles/{id}/accounts",
            post(admin::create_sub_account),
        )
        // Skill management
        .route(
            "/api/admin/profiles/{id}/skills",
            get(admin::list_profile_skills),
        )
        .route(
            "/api/admin/profiles/{id}/skills",
            post(admin::install_profile_skill),
        )
        .route(
            "/api/admin/profiles/{id}/skills/{name}",
            delete(admin::remove_profile_skill),
        )
        // User management
        .route("/api/admin/users", get(user_admin::list_users))
        .route("/api/admin/users", post(user_admin::create_user))
        .route("/api/admin/users/{id}", delete(user_admin::delete_user))
        // Session & cron diagnostics
        .route(
            "/api/admin/profiles/{id}/sessions",
            get(admin::list_sessions),
        )
        .route(
            "/api/admin/profiles/{id}/sessions/read",
            get(admin::read_session),
        )
        .route("/api/admin/profiles/{id}/cron", get(admin::list_cron_jobs))
        .route(
            "/api/admin/profiles/{id}/config-check",
            get(admin::config_check),
        )
        // System metrics
        .route("/api/admin/system/metrics", get(admin::system_metrics))
        // Monitor control
        .route("/api/admin/monitor/status", get(admin::monitor_status))
        .route("/api/admin/monitor/watchdog", post(admin::toggle_watchdog))
        .route("/api/admin/monitor/alerts", post(admin::toggle_alerts))
        // Platform skills management
        .route(
            "/api/admin/platform-skills",
            get(admin::list_platform_skills),
        )
        .route(
            "/api/admin/platform-skills/{name}/install",
            post(admin::install_platform_skill),
        )
        .route(
            "/api/admin/platform-skills/{name}",
            delete(admin::remove_platform_skill),
        )
        .route(
            "/api/admin/platform-skills/{name}/health",
            get(admin::platform_skill_health),
        )
        // ominix-api service management
        .route(
            "/api/admin/platform-skills/ominix-api/start",
            post(admin::platform_service_start),
        )
        .route(
            "/api/admin/platform-skills/ominix-api/stop",
            post(admin::platform_service_stop),
        )
        .route(
            "/api/admin/platform-skills/ominix-api/restart",
            post(admin::platform_service_restart),
        )
        .route(
            "/api/admin/platform-skills/ominix-api/logs",
            get(admin::platform_service_logs),
        )
        // Model management (proxy to ominix-api)
        .route(
            "/api/admin/platform-skills/ominix-api/models",
            get(admin::platform_models_catalog),
        )
        .route(
            "/api/admin/platform-skills/ominix-api/models/download",
            post(admin::platform_models_download),
        )
        .route(
            "/api/admin/platform-skills/ominix-api/models/remove",
            post(admin::platform_models_remove),
        )
        .route(
            "/api/admin/platform-skills/ominix-api/models/available",
            get(admin::platform_models_available),
        )
        .route(
            "/api/admin/platform-skills/ominix-api/models/enable",
            post(admin::platform_models_enable),
        )
        .route(
            "/api/admin/platform-skills/ominix-api/models/disable",
            post(admin::platform_models_disable),
        )
        // System update
        .route("/api/admin/system/version", post(admin::system_version))
        .route("/api/admin/system/update", post(admin::system_update))
        // Model limits (from model_limits.json)
        .route("/api/admin/model-limits", get(admin::model_limits))
        // Tunnel tenant management
        .route("/api/admin/tenants", get(admin::list_tenants))
        .route("/api/admin/tenants", post(admin::create_tenant))
        .route("/api/admin/tenants/{id}", get(admin::get_tenant))
        .route("/api/admin/tenants/{id}", delete(admin::delete_tenant))
        .route(
            "/api/admin/tenants/{id}/setup-script",
            get(admin::tenant_setup_script),
        );

    // Conditionally enable admin shell endpoint (disabled by default).
    let admin_api = if state.allow_admin_shell {
        tracing::warn!(
            "admin shell endpoint enabled (POST /api/admin/shell). \
             Disable with allow_admin_shell = false in config for production."
        );
        admin_api.route("/api/admin/shell", post(admin::admin_shell))
    } else {
        admin_api
    };

    // Determine whether auth middleware is needed
    let has_auth = state.auth_token.is_some() || state.auth_manager.is_some();

    // Build the authenticated routes
    let protected = if has_auth {
        // Routes requiring user-level auth (user session OR admin token)
        let user_routes = my_api.merge(chat_api).layer(middleware::from_fn_with_state(
            state.clone(),
            user_auth_middleware,
        ));

        // Routes requiring admin-level auth (admin token only)
        let admin_routes = admin_api.layer(middleware::from_fn_with_state(
            state.clone(),
            admin_auth_middleware,
        ));

        user_routes.merge(admin_routes)
    } else {
        // No auth configured — all routes accessible
        my_api.merge(chat_api).merge(admin_api)
    };

    // Webhook proxy routes (unauthenticated — Feishu/Twilio servers can't authenticate)
    let webhook_routes = Router::new()
        .route(
            "/webhook/feishu/{profile_id}",
            post(webhook_proxy::feishu_webhook_proxy),
        )
        .route(
            "/webhook/twilio/{profile_id}",
            post(webhook_proxy::twilio_webhook_proxy),
        );

    // Metrics route — protected when auth is configured, public otherwise
    let metrics_route = Router::new().route("/metrics", get(metrics::metrics_handler));
    let metrics_route = if has_auth {
        metrics_route.layer(middleware::from_fn_with_state(
            state.clone(),
            user_auth_middleware,
        ))
    } else {
        metrics_route
    };

    // Public version/health endpoints (no auth required)
    let version_routes = Router::new()
        .route("/api/version", get(handlers::version))
        .route("/health", get(handlers::health));

    // Internal endpoint for frps server plugin (no auth — called by frps on localhost)
    let internal_routes =
        Router::new().route("/api/internal/frps-auth", post(frps_plugin::frps_auth));

    // Unauthenticated routes (static files + auth endpoints + webhook proxy + internal)
    let public = Router::new()
        .merge(metrics_route)
        .merge(auth_api)
        .route(
            "/api/preview/{profile_id}/{session_id}/{site_slug}",
            get(handlers::serve_public_site_preview_root),
        )
        .route(
            "/api/preview/{profile_id}/{session_id}/{site_slug}/",
            get(handlers::serve_public_site_preview_root),
        )
        .route(
            "/api/preview/{profile_id}/{session_id}/{site_slug}/{*path}",
            get(handlers::serve_public_site_preview_path),
        )
        .route(
            "/api/register/setup-script/{id}/{auth_token}",
            get(admin::register_setup_script_public),
        )
        .merge(webhook_routes)
        .merge(version_routes)
        .merge(internal_routes);

    public
        .merge(protected)
        .fallback(static_files::static_handler)
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(state)
}

/// Constant-time byte comparison to prevent timing attacks on auth tokens (no length leak).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let len_eq = a.len() ^ b.len();
    let mut result = 0u8;
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        result |= x ^ y;
    }
    result == 0 && len_eq == 0
}

/// Extract bearer token from request headers or query params.
fn extract_token(req: &axum::http::Request<axum::body::Body>) -> String {
    // Try Authorization header first
    let header_token = req
        .headers()
        .get("authorization")
        .and_then(|v: &HeaderValue| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or("");

    // Fall back to ?token= or ?_token= query param (for SSE / EventSource / img tags)
    let query_token = req
        .uri()
        .query()
        .and_then(|q| {
            q.split('&').find_map(|pair| {
                pair.strip_prefix("token=")
                    .or_else(|| pair.strip_prefix("_token="))
            })
        })
        .unwrap_or("");

    if !header_token.is_empty() {
        header_token.to_string()
    } else {
        query_token.to_string()
    }
}

/// Resolve token to an AuthIdentity.
async fn resolve_identity(state: &AppState, token: &str) -> Option<AuthIdentity> {
    if token.is_empty() {
        return None;
    }

    // 1. Check admin token (constant-time)
    if let Some(expected) = &state.auth_token {
        if constant_time_eq(token.as_bytes(), expected.as_bytes()) {
            return Some(AuthIdentity::Admin);
        }
    }

    // 1b. Check OCTOS_TEST_TOKEN for e2e test auth bypass
    if let Ok(test_token) = std::env::var("OCTOS_TEST_TOKEN") {
        if !test_token.is_empty() && constant_time_eq(token.as_bytes(), test_token.as_bytes()) {
            return Some(AuthIdentity::User {
                id: "e2e-test".into(),
                role: UserRole::User,
            });
        }
    }

    // 2. Check user session
    if let Some(ref auth_mgr) = state.auth_manager {
        if let Some((user_id, role)) = auth_mgr.validate_session(token).await {
            return Some(AuthIdentity::User { id: user_id, role });
        }
    }

    None
}

/// Auth middleware for user-level access (user session or admin token).
///
/// Also accepts `X-Profile-Id` header as authentication for the chat API
/// routes when accessed through a reverse proxy (e.g. Caddy with per-profile
/// subdomains). The proxy sets this header to identify the profile.
async fn user_auth_middleware(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    mut req: axum::http::Request<axum::body::Body>,
    next: Next,
) -> Result<axum::response::Response, StatusCode> {
    let token = extract_token(&req);
    let method = req.method().clone();
    let uri = req.uri().clone();
    let profile_id = req
        .headers()
        .get("x-profile-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // 1. Try token-based auth (admin token or OTP session)
    if let Some(identity) = resolve_identity(&state, &token).await {
        req.extensions_mut().insert(identity);
        return Ok(next.run(req).await);
    }

    // 2. Accept X-Profile-Id header for chat API routes (proxy auth).
    // The reverse proxy (Caddy) sets this header to identify the profile,
    // so requests through the proxy are implicitly authenticated.
    // SECURITY: Only accept this header from loopback addresses to prevent
    // profile impersonation via misconfigured reverse proxy or SSRF.
    let is_loopback = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip().is_loopback())
        .unwrap_or(false);

    if !profile_id.is_empty() && is_loopback {
        // Validate that the profile actually exists to prevent spoofing.
        if let Some(ref store) = state.profile_store {
            if store.get(&profile_id).ok().flatten().is_none() {
                tracing::warn!(profile_id = %profile_id, "X-Profile-Id references non-existent profile");
                return Err(StatusCode::UNAUTHORIZED);
            }
        }

        let uri_str = uri.path();
        // Only allow proxy auth for chat-related endpoints, not admin
        if uri_str.starts_with("/api/chat")
            || uri_str.starts_with("/api/ws")
            || uri_str.starts_with("/api/upload")
            || uri_str.starts_with("/api/sessions")
            || uri_str.starts_with("/api/files")
            || uri_str.starts_with("/api/status")
        {
            req.extensions_mut().insert(AuthIdentity::User {
                id: profile_id,
                role: UserRole::User,
            });
            return Ok(next.run(req).await);
        }
    }

    if !profile_id.is_empty() && !is_loopback {
        tracing::warn!(
            profile_id = %profile_id,
            "X-Profile-Id header rejected: request not from loopback address"
        );
    }

    tracing::warn!(
        method = %method,
        uri = %uri,
        token_len = token.len(),
        token_prefix = %if token.len() > 8 { &token[..8] } else { &token },
        "user auth rejected"
    );
    Err(StatusCode::UNAUTHORIZED)
}

/// Auth middleware for admin-level access (admin token only, or admin role user).
async fn admin_auth_middleware(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    mut req: axum::http::Request<axum::body::Body>,
    next: Next,
) -> Result<axum::response::Response, StatusCode> {
    let token = extract_token(&req);
    let method = req.method().clone();
    let uri = req.uri().clone();

    match resolve_identity(&state, &token).await {
        Some(AuthIdentity::Admin) => {
            req.extensions_mut().insert(AuthIdentity::Admin);
            Ok(next.run(req).await)
        }
        Some(AuthIdentity::User {
            role: UserRole::Admin,
            id,
        }) => {
            req.extensions_mut().insert(AuthIdentity::User {
                id,
                role: UserRole::Admin,
            });
            Ok(next.run(req).await)
        }
        _ => {
            tracing::warn!(
                method = %method,
                uri = %uri,
                token_len = token.len(),
                token_prefix = %if token.len() > 8 { &token[..8] } else { &token },
                "admin auth rejected"
            );
            Err(StatusCode::UNAUTHORIZED)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{AppState, SseBroadcaster};
    use crate::config::DeploymentMode;
    use crate::tenant::{TenantConfig, TenantStatus, TenantStore};
    use axum::http::Request;
    use chrono::Utc;
    use std::sync::Arc;

    #[test]
    fn test_constant_time_eq_equal() {
        assert!(constant_time_eq(b"secret-token", b"secret-token"));
    }

    #[test]
    fn test_constant_time_eq_not_equal() {
        assert!(!constant_time_eq(b"secret-token", b"wrong-token!"));
    }

    #[test]
    fn test_constant_time_eq_different_lengths() {
        assert!(!constant_time_eq(b"short", b"longer-string"));
    }

    #[test]
    fn test_constant_time_eq_empty() {
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn test_constant_time_eq_single_bit_diff() {
        assert!(!constant_time_eq(b"\x00", b"\x01"));
    }

    #[test]
    fn extract_token_from_bearer_header() {
        let req = Request::builder()
            .header("authorization", "Bearer my-token")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(extract_token(&req), "my-token");
    }

    #[test]
    fn extract_token_from_query_param() {
        let req = Request::builder()
            .uri("/api/chat/stream?token=query-tok")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(extract_token(&req), "query-tok");
    }

    #[test]
    fn extract_token_header_takes_precedence() {
        let req = Request::builder()
            .uri("/api/chat/stream?token=query-tok")
            .header("authorization", "Bearer header-tok")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(extract_token(&req), "header-tok");
    }

    #[test]
    fn extract_token_no_auth_returns_empty() {
        let req = Request::builder()
            .uri("/api/status")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(extract_token(&req), "");
    }

    #[test]
    fn extract_token_wrong_scheme_returns_empty() {
        let req = Request::builder()
            .header("authorization", "Basic abc123")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(extract_token(&req), "");
    }

    #[test]
    fn extract_token_query_with_other_params() {
        let req = Request::builder()
            .uri("/api/stream?foo=bar&token=tok123&baz=1")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(extract_token(&req), "tok123");
    }

    #[test]
    fn extract_token_query_no_token_param() {
        let req = Request::builder()
            .uri("/api/stream?foo=bar&baz=1")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(extract_token(&req), "");
    }

    #[tokio::test]
    async fn public_register_setup_script_route_bypasses_user_auth() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(TenantStore::open(dir.path()).unwrap());
        let now = Utc::now();
        store
            .save(&TenantConfig {
                id: "edward".into(),
                name: "edward".into(),
                subdomain: "edward".into(),
                tunnel_token: String::new(),
                ssh_port: 6001,
                local_port: 8080,
                auth_token: "public-auth-token".into(),
                owner: "edward".into(),
                status: TenantStatus::Pending,
                created_at: now,
                updated_at: now,
            })
            .unwrap();

        let state = Arc::new(AppState {
            agent: None,
            sessions: None,
            broadcaster: Arc::new(SseBroadcaster::new(16)),
            started_at: Utc::now(),
            auth_token: Some("admin-secret".into()),
            metrics_handle: None,
            profile_store: None,
            process_manager: None,
            user_store: None,
            auth_manager: None,
            http_client: reqwest::Client::new(),
            config_path: None,
            watchdog_enabled: None,
            alerts_enabled: None,
            sysinfo: tokio::sync::Mutex::new(sysinfo::System::new()),
            tenant_store: Some(store),
            tunnel_domain: Some("octos-cloud.org".into()),
            frps_server: Some("127.0.0.1".into()),
            frps_port: Some(7000),
            deployment_mode: DeploymentMode::Cloud,
            allow_admin_shell: false,
            content_catalog_mgr: None,
        });

        let app = build_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap();
        });
        tokio::task::yield_now().await;

        let response = reqwest::Client::new()
            .get(format!(
                "http://{addr}/api/register/setup-script/edward/public-auth-token"
            ))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.unwrap();
        assert!(body.contains("install.sh"));

        server.abort();
    }
}
