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
use super::admin_setup;
use super::auth_handlers;
use super::events_harness;
use super::frps_plugin;
use super::handlers;
use super::metrics;
use super::purge;
use super::static_files;
use super::swarm as swarm_api;
use super::ui_protocol;
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

/// Backward-compatible default when the operator has not configured a
/// base domain via `config.base_domain` or `OCTOS_BASE_DOMAIN`.
pub const DEFAULT_BASE_DOMAIN: &str = "crew.ominix.io";

/// Compose the CORS allowlist for a given base domain.
///
/// The returned list always contains the bare-`ominix.io` entries and
/// the loopback dev origins, plus the three `app./admin./api.` entries
/// for the configured base domain. `None` falls back to the legacy
/// `crew.ominix.io` triple so existing minis keep working without config
/// changes.
pub fn cors_allowlist_for_base_domain(base: Option<&str>) -> Vec<String> {
    let base = base.unwrap_or(DEFAULT_BASE_DOMAIN);
    vec![
        "https://app.ominix.io".to_string(),
        "https://admin.ominix.io".to_string(),
        "https://api.ominix.io".to_string(),
        format!("https://app.{base}"),
        format!("https://admin.{base}"),
        format!("https://api.{base}"),
        "http://localhost:3000".to_string(),
        "http://localhost:5173".to_string(),
    ]
}

/// Build the axum router with all API routes.
pub fn build_router(state: Arc<AppState>) -> Router {
    // Restrict CORS to an explicit allowlist of known origins.
    // Do NOT use suffix matching (e.g. ends_with(".ominix.io")) — a hijacked
    // subdomain would pass the check and enable cross-origin requests.
    //
    // The allowlist is composed from `state.base_domain` at startup so each
    // mini accepts its own public subdomain variants (`crew.`, `bot.`,
    // `octos.`, `ocean.`) without redeploys. `None` preserves the legacy
    // `crew.ominix.io` triple.
    let allowed_origins: Arc<Vec<String>> =
        Arc::new(cors_allowlist_for_base_domain(state.base_domain.as_deref()));
    let cors = {
        let allowed = allowed_origins.clone();
        CorsLayer::new()
            .allow_origin(tower_http::cors::AllowOrigin::predicate(
                move |origin, _| {
                    let o = origin.to_str().unwrap_or("");
                    allowed.iter().any(|s| s == o)
                },
            ))
            .allow_methods(tower_http::cors::Any)
            .allow_headers(tower_http::cors::Any)
    };

    // Public auth endpoints (no auth required)
    let auth_api = Router::new()
        .route("/api/auth/status", get(auth_handlers::auth_status))
        .route("/api/auth/send-code", post(auth_handlers::send_code))
        .route("/api/auth/verify", post(auth_handlers::verify))
        .route("/api/auth/logout", post(auth_handlers::logout));

    // Chat + status API (existing)
    //
    // Transport history:
    // - M9-α-5/α-6 (ADR PR #830 / audit issue #845): the chat SSE
    //   transport (`POST /api/chat?stream=true`, `GET /api/chat/stream`,
    //   `GET /api/sessions/:id/events/stream`) was deleted.
    // - Cleanup PR #908: the legacy text-frame `/api/ws` was retired
    //   (no live clients and it never carried the UI Protocol v1 wire
    //   format).
    // - Cleanup follow-up to #908: the surviving sync REST endpoint
    //   `POST /api/chat` was retired once the last callers
    //   (coding_multi_session integration test, three e2e specs,
    //   validate-m4-1a-live.sh) migrated to the canonical WS path.
    //
    // The sole chat transport is `/api/ui-protocol/ws`. The
    // harness/admin `/api/events/harness` SSE endpoint is unrelated to
    // chat and remains.
    //
    // M12 Phase D-5 (ADR PR #910 / audit PR #911): the auxiliary
    // session/status REST surface has been retired and replaced by the
    // WS UI Protocol v1 RPC methods on `/api/ui-protocol/ws`:
    //
    //   GET    /api/sessions                          → session/list
    //   GET    /api/sessions/{id}/messages            → session/messages_page
    //   GET    /api/sessions/{id}/status              → session/status.get
    //   GET    /api/sessions/{id}/files               → session/files.list
    //   GET    /api/sessions/{id}/tasks               → session/tasks.list
    //   GET    /api/sessions/{id}/workspace-contract  → session/workspace.get
    //   PATCH  /api/sessions/{id}/title               → session/title.set
    //   DELETE /api/sessions/{id}                     → session/delete
    //   GET    /api/status                            → system/status.get
    //
    // The handler functions are retained as private helpers because the
    // WS dispatcher in `ui_protocol.rs` still reuses them to back the
    // RPC methods above; only the REST route registrations are dropped.
    // Auth (`/api/auth/*`), blob (`/api/files/*`), task-control
    // (`/api/tasks/*`), chat ingress (`/api/ui-protocol/ws`), uploads,
    // and site-preview remain REST per the ADR.
    let chat_api = Router::new()
        .route("/api/events/harness", get(events_harness::events_harness))
        .route("/api/ui-protocol/ws", get(ui_protocol::ws_handler))
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
        // Issue #994 (P0 sev2 cross-tenant data read): these routes
        // used to live on the unauthenticated `public` branch below
        // and resolved profile + session purely from the URL tuple.
        // They now require user auth and the handler asserts that
        // the authenticated identity owns the route's `profile_id`
        // AND the route's `session_id` resolves to a workspace under
        // that profile's data directory — see
        // [`handlers::serve_owned_site_preview_root`].
        .route(
            "/api/preview/{profile_id}/{session_id}/{site_slug}",
            get(handlers::serve_owned_site_preview_root),
        )
        .route(
            "/api/preview/{profile_id}/{session_id}/{site_slug}/",
            get(handlers::serve_owned_site_preview_root),
        )
        .route(
            "/api/preview/{profile_id}/{session_id}/{site_slug}/{*path}",
            get(handlers::serve_owned_site_preview_path),
        )
        .route("/api/files/list", get(handlers::list_content_files))
        .route("/api/files/{filename}", get(handlers::serve_file))
        .route("/api/files", get(handlers::serve_file_by_query))
        // M7.9 / W2 — task supervisor exposure (kept REST)
        .route("/api/tasks/{task_id}/cancel", post(handlers::cancel_task))
        .route(
            "/api/tasks/{task_id}/restart-from-node",
            post(handlers::restart_task_from_node),
        );

    // User self-service endpoints (user or admin auth)
    let my_api = Router::new()
        .route("/api/my/profile", get(auth_handlers::my_profile))
        .route("/api/my/profile", put(auth_handlers::update_my_profile))
        .route("/api/my/soul", get(auth_handlers::my_soul))
        .route("/api/my/soul", put(auth_handlers::update_my_soul))
        .route("/api/my/soul", delete(auth_handlers::delete_my_soul))
        // M12 Phase D-5: `/api/my/content` (list), `/api/my/content/{id}`
        // (delete), and `/api/my/content/bulk-delete` retired in favor of
        // WS RPC methods `content/list`, `content/delete`, and
        // `content/bulk_delete` on `/api/ui-protocol/ws`. The blob read
        // endpoints (`/{id}/thumbnail`, `/{id}/body`) remain REST per ADR.
        .route(
            "/api/my/content/{id}/thumbnail",
            get(auth_handlers::my_content_thumbnail),
        )
        .route(
            "/api/my/content/{id}/body",
            get(auth_handlers::my_content_body),
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
        .route(
            "/api/my/profile/skills",
            get(auth_handlers::my_profile_skills),
        )
        .route(
            "/api/my/profile/skills/registry",
            get(auth_handlers::my_profile_skill_registry),
        )
        .route(
            "/api/my/profile/skills",
            post(auth_handlers::install_my_profile_skill),
        )
        .route(
            "/api/my/profile/skills/{name}",
            delete(auth_handlers::remove_my_profile_skill),
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
            "/api/my/profile/accounts",
            post(auth_handlers::create_my_sub_account),
        )
        .route(
            "/api/my/profile/accounts/{id}",
            get(auth_handlers::my_sub_account),
        )
        .route(
            "/api/my/profile/accounts/{id}",
            put(auth_handlers::update_my_sub_account),
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
        .route(
            "/api/admin/profiles/{id}/purge",
            post(purge::purge_profile_handler),
        )
        .route(
            "/api/admin/profiles/by-node/{node_name}/purge",
            post(purge::purge_by_node_handler),
        )
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
        // First-run setup wizard
        .route("/api/admin/token/status", get(admin_setup::token_status))
        .route("/api/admin/token/rotate", post(admin_setup::rotate_token))
        .route(
            "/api/admin/token/email",
            post(admin_setup::post_token_email),
        )
        .route("/api/admin/setup/state", get(admin_setup::get_setup_state))
        .route("/api/admin/setup/step", post(admin_setup::post_setup_step))
        .route(
            "/api/admin/setup/complete",
            post(admin_setup::post_setup_complete),
        )
        .route("/api/admin/setup/skip", post(admin_setup::post_setup_skip))
        // SMTP configuration
        .route("/api/admin/smtp", get(admin_setup::get_smtp))
        .route("/api/admin/smtp", post(admin_setup::post_smtp))
        .route("/api/admin/smtp/test", post(admin_setup::post_smtp_test))
        // Deployment mode
        .route(
            "/api/admin/deployment-mode",
            get(admin_setup::get_deployment_mode),
        )
        .route(
            "/api/admin/deployment-mode",
            post(admin_setup::post_deployment_mode),
        )
        .route(
            "/api/admin/deployment-mode/detect",
            get(admin_setup::get_deployment_mode_detect),
        )
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
        .route("/api/admin/users/{id}", delete(user_admin::delete_user))
        .route(
            "/api/admin/allowed-emails",
            get(user_admin::list_allowed_emails),
        )
        .route(
            "/api/admin/allowed-emails",
            post(user_admin::add_allowed_email),
        )
        .route(
            "/api/admin/allowed-emails/{email}",
            delete(user_admin::delete_allowed_email),
        )
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
        .route("/api/admin/operator/summary", get(admin::operator_summary))
        .route("/api/admin/operator/tasks", get(admin::operator_tasks))
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
        )
        // M7.6 — contract-authoring + swarm dispatch dashboard
        .route("/api/swarm/dispatch", post(swarm_api::dispatch_swarm))
        .route("/api/swarm/dispatches", get(swarm_api::list_dispatches))
        .route(
            "/api/swarm/dispatches/{id}",
            get(swarm_api::dispatch_detail),
        )
        .route(
            "/api/swarm/dispatches/{id}/review",
            post(swarm_api::submit_review),
        )
        .route(
            "/api/cost/attributions/{dispatch_id}",
            get(swarm_api::cost_attributions),
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
    //
    // Issue #994 (P0 sev2 cross-tenant data read): `/api/preview/...`
    // used to live here. It now sits on the authenticated `chat_api`
    // group above — the handler asserts identity-owns-profile +
    // session-belongs-to-profile, so the URL tuple is no longer
    // sufficient to read another tenant's built site.
    let public = Router::new()
        .merge(metrics_route)
        .merge(auth_api)
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

    // 1. Check hashed admin-token store. When the file is present, it is
    //    authoritative for admin auth — the bootstrap token no longer works
    //    until an operator runs `octos admin reset-token`. A corrupt file
    //    fails closed (admin branch disabled; fall through to user-session).
    match state.admin_token_store.load() {
        Ok(Some(record)) => {
            if record.verify(token) {
                return Some(AuthIdentity::Admin);
            }
        }
        Ok(None) => {
            // 1a. No rotation yet — fall back to the config/env bootstrap token.
            if let Some(expected) = &state.auth_token {
                if constant_time_eq(token.as_bytes(), expected.as_bytes()) {
                    return Some(AuthIdentity::Admin);
                }
            }
        }
        Err(e) => {
            tracing::error!(error = ?e, "admin_token.json could not be loaded; admin auth disabled until fixed");
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
        // Allow proxy auth for chat- and session-scoped endpoints, not admin.
        // Task-control verbs (`/api/tasks/{id}/cancel`, `/restart-from-node`)
        // are session-scoped — same trust posture as files/uploads. The
        // legacy `/api/sessions`, `/api/status`, and `/api/chat` prefixes
        // used to live here too; they were dropped together with the
        // routes (M12 Phase D-5 retired sessions/status; the #908
        // follow-up retired chat).
        if uri_str.starts_with("/api/ui-protocol")
            || uri_str.starts_with("/api/upload")
            || uri_str.starts_with("/api/files")
            || uri_str.starts_with("/api/tasks")
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
    use crate::api::AppState;
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
            .uri("/api/ui-protocol/ws?token=query-tok")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(extract_token(&req), "query-tok");
    }

    #[test]
    fn extract_token_header_takes_precedence() {
        let req = Request::builder()
            .uri("/api/ui-protocol/ws?token=query-tok")
            .header("authorization", "Bearer header-tok")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(extract_token(&req), "header-tok");
    }

    #[test]
    fn extract_token_no_auth_returns_empty() {
        // Any URI works — `extract_token` only consults headers and the
        // query string. `/api/version` is a stable surviving REST surface
        // after the M12 Phase D-5 retirement of `/api/status`.
        let req = Request::builder()
            .uri("/api/version")
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
            auth_token: Some("admin-secret".into()),
            admin_token_store: Arc::new(crate::admin_token_store::AdminTokenStore::new(dir.path())),
            setup_state_store: Arc::new(crate::setup_state_store::SetupStateStore::new(dir.path())),
            tenant_store: Some(store),
            tunnel_domain: Some("octos-cloud.org".into()),
            base_domain: None,
            frps_server: Some("127.0.0.1".into()),
            frps_port: Some(7000),
            deployment_mode: DeploymentMode::Cloud,
            ..AppState::empty_for_tests()
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

    /// Build a minimal `AppState` for resolve_identity tests — only the
    /// fields consulted by admin auth are populated.
    fn identity_state(data_dir: &std::path::Path, bootstrap: Option<&str>) -> AppState {
        AppState {
            auth_token: bootstrap.map(|s| s.to_string()),
            admin_token_store: Arc::new(crate::admin_token_store::AdminTokenStore::new(data_dir)),
            ..AppState::empty_for_tests()
        }
    }

    #[tokio::test]
    async fn resolve_identity_accepts_bootstrap_when_no_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let state = identity_state(dir.path(), Some("boot"));
        assert!(matches!(
            resolve_identity(&state, "boot").await,
            Some(AuthIdentity::Admin)
        ));
    }

    #[tokio::test]
    async fn resolve_identity_prefers_rotated_record_and_rejects_bootstrap() {
        use crate::admin_token_store::{AdminTokenRecord, AdminTokenStore};
        let dir = tempfile::tempdir().unwrap();
        let store = AdminTokenStore::new(dir.path());
        store
            .save(&AdminTokenRecord::from_plaintext("rotated"))
            .unwrap();

        let state = identity_state(dir.path(), Some("boot"));

        assert!(matches!(
            resolve_identity(&state, "rotated").await,
            Some(AuthIdentity::Admin)
        ));
        // Bootstrap token must NOT work once rotated.
        assert!(resolve_identity(&state, "boot").await.is_none());
    }

    #[tokio::test]
    async fn resolve_identity_fails_closed_on_corrupt_admin_token_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("admin_token.json"), b"{not json").unwrap();

        let state = identity_state(dir.path(), Some("boot"));

        // With a corrupt file, neither the bootstrap nor any other token
        // resolves to Admin — admin auth is disabled until an operator fixes
        // the file.
        assert!(resolve_identity(&state, "boot").await.is_none());
        assert!(resolve_identity(&state, "rotated").await.is_none());
    }

    /// Bug 1 regression: `GET /api/events/harness` with a valid admin
    /// Bearer token must return `200 text/event-stream`, NOT the
    /// `307 Location: /admin/` that the SPA static-file fallback was
    /// emitting for this documented-but-unwired endpoint. Live-sweep
    /// against release/coding-blue surfaced this as Playwright's
    /// `apiRequestContext.get: Max redirect count exceeded`.
    #[tokio::test]
    async fn events_harness_route_with_bearer_auth_returns_200_sse() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(AppState {
            auth_token: Some("admin-secret".into()),
            admin_token_store: Arc::new(crate::admin_token_store::AdminTokenStore::new(dir.path())),
            setup_state_store: Arc::new(crate::setup_state_store::SetupStateStore::new(dir.path())),
            ..AppState::empty_for_tests()
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

        let response = reqwest::Client::builder()
            // Catch any lingering 307 as an explicit failure rather than
            // letting reqwest silently follow it to `/admin/`.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
            .get(format!(
                "http://{addr}/api/events/harness?kinds=swarm_dispatch"
            ))
            .bearer_auth("admin-secret")
            .send()
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "bearer-authed /api/events/harness must return 200 (not 307 to /admin/)"
        );
        let ct = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            ct.starts_with("text/event-stream"),
            "content-type must be text/event-stream, got {ct:?}"
        );

        server.abort();
    }

    /// Bug 1 regression (fallback side): an unknown `/api/*` path reaches
    /// the SPA fallback because no route matches. It must return
    /// `404 application/json`, NOT `307 Location: /admin/`, so API
    /// clients see a typed error instead of being redirected into the
    /// SPA.
    #[tokio::test]
    async fn unmatched_api_path_returns_json_404_not_redirect() {
        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(AppState {
            admin_token_store: Arc::new(crate::admin_token_store::AdminTokenStore::new(dir.path())),
            setup_state_store: Arc::new(crate::setup_state_store::SetupStateStore::new(dir.path())),
            ..AppState::empty_for_tests()
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

        let response = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap()
            .get(format!("http://{addr}/api/definitely-not-a-route"))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let ct = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(ct.starts_with("application/json"), "got {ct:?}");
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["error"], "not_found");

        server.abort();
    }

    #[test]
    fn should_compose_cors_allowlist_from_base_domain() {
        let list = cors_allowlist_for_base_domain(Some("bot.ominix.io"));
        assert!(
            list.contains(&"https://app.bot.ominix.io".to_string()),
            "missing app.bot.ominix.io in {list:?}"
        );
        assert!(
            list.contains(&"https://admin.bot.ominix.io".to_string()),
            "missing admin.bot.ominix.io in {list:?}"
        );
        assert!(
            list.contains(&"https://api.bot.ominix.io".to_string()),
            "missing api.bot.ominix.io in {list:?}"
        );
        // The bare ominix.io entries remain for shared landing pages.
        assert!(list.contains(&"https://app.ominix.io".to_string()));
    }

    #[test]
    fn should_default_cors_to_crew_ominix_io_when_unset() {
        // Backward-compat: when no base_domain is configured the server
        // must still accept the historical `*.crew.ominix.io` origins so
        // existing minis keep working without a config change.
        let list = cors_allowlist_for_base_domain(None);
        assert!(list.contains(&"https://app.crew.ominix.io".to_string()));
        assert!(list.contains(&"https://admin.crew.ominix.io".to_string()));
        assert!(list.contains(&"https://api.crew.ominix.io".to_string()));
    }

    #[test]
    fn should_not_accept_unrelated_origin_in_base_domain_allowlist() {
        // Defence-in-depth: a subdomain of a different tenant must never
        // appear in the composed list even when a base_domain is set.
        let list = cors_allowlist_for_base_domain(Some("bot.ominix.io"));
        assert!(!list.iter().any(|s| s.contains("evil.example.com")));
        assert!(!list.iter().any(|s| s.contains("ocean.ominix.io")));
    }
}
