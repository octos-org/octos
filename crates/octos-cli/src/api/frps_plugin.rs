//! frps server plugin handler for per-tenant tunnel authorization.
//!
//! frps sends HTTP requests to this endpoint for Login and NewProxy operations.
//! Login authenticates the tenant via `metadatas.token` (set in frpc.toml),
//! and NewProxy revalidates `user.metas.token` against the current tenant store.
//! Run IDs are assigned after first Login and are not authorization evidence.
//!
//! Both frps and frpc set `auth.token = ""` so frps's built-in VerifyLogin is
//! a trivial no-op md5("" + ts) match — it cannot carry the per-tenant secret,
//! which is why the token rides in metadatas instead. Returning `unchange:true`
//! and not rewriting the Login content is the same pattern used by production
//! plugins like frp-auth-plugin; rewriting content (`unchange:false`) passes
//! VerifyLogin but breaks the control session post-login.
//!
//! Protocol: <https://github.com/fatedier/frp/blob/dev/doc/server_plugin.md>

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use super::AppState;

const LOGIN_TIMESTAMP_MAX_DRIFT_SECS: i64 = 15 * 60;
const RUN_ID_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// frps plugin request envelope.
#[derive(Debug, Deserialize)]
pub struct PluginRequest {
    pub op: String,
    pub content: serde_json::Value,
}

/// frps plugin response envelope.
#[derive(Debug, Serialize)]
pub struct PluginResponse {
    pub reject: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reject_reason: Option<String>,
    pub unchange: bool,
}

impl PluginResponse {
    fn allow() -> Self {
        Self {
            reject: false,
            reject_reason: None,
            unchange: true,
        }
    }

    fn deny(reason: impl Into<String>) -> Self {
        Self {
            reject: true,
            reject_reason: Some(reason.into()),
            unchange: false,
        }
    }
}

/// Login content from frps — sent when a client connects.
#[derive(Debug, Deserialize)]
struct LoginContent {
    /// Unix timestamp sent by frpc (integer or string depending on frp version).
    #[serde(default, deserialize_with = "deserialize_timestamp")]
    timestamp: String,
    /// Unique identifier for this frpc session.
    #[serde(default)]
    run_id: String,
    /// Tenant metadata from `metadatas` in frpc.toml (wire field: `metas`).
    /// We expect `metas.token = <per-tenant-uuid>`.
    #[serde(default)]
    metas: std::collections::HashMap<String, String>,
}

fn deserialize_timestamp<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let v: serde_json::Value = serde::Deserialize::deserialize(d)?;
    match v {
        serde_json::Value::String(s) => Ok(s),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        _ => Err(<D::Error as serde::de::Error>::custom(
            "timestamp must be a string or number",
        )),
    }
}

/// NewProxy content from frps — contains the proxy configuration being registered.
#[derive(Debug, Deserialize)]
struct NewProxyContent {
    /// Login metadata copied into the user envelope by frps, not proxy metas.
    #[serde(default)]
    user: Option<ProxyUser>,
    /// Proxy type (http, tcp, etc.).
    #[serde(default)]
    proxy_type: String,
    /// Custom domains for HTTP proxies.
    #[serde(default)]
    custom_domains: Vec<String>,
    /// Alternate FRP subdomain registration is not supported.
    #[serde(default)]
    subdomain: String,
    /// Remote port for TCP proxies.
    #[serde(default)]
    remote_port: u16,
}

#[derive(Debug, Deserialize)]
struct ProxyUser {
    #[serde(default)]
    metas: std::collections::HashMap<String, String>,
}

/// POST /api/internal/frps-auth — frps server plugin handler.
///
/// Handles two operations:
/// - `Login`: Authenticates the per-tenant metadata token.
/// - `NewProxy`: Reauthenticates that token and checks every requested resource.
pub async fn frps_auth(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    Json(req): Json<PluginRequest>,
) -> Result<Json<PluginResponse>, (StatusCode, Json<PluginResponse>)> {
    // Only allow requests from localhost (frps runs on the same machine)
    if !addr.ip().is_loopback() {
        tracing::warn!(remote = %addr, "frps auth plugin called from non-loopback address");
        return Err((
            StatusCode::FORBIDDEN,
            Json(PluginResponse::deny("forbidden")),
        ));
    }

    let store = match state.tenant_store.as_ref() {
        Some(s) => s,
        None => {
            tracing::error!("frps auth plugin called but tenant store not configured");
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(PluginResponse::deny("tenant store not configured")),
            ));
        }
    };

    match req.op.as_str() {
        "Login" => {
            // Per-tenant auth via metadatas.token — both frps and frpc set
            // `auth.token = ""`, so the built-in token check is a trivial
            // md5("" + ts) == md5("" + ts) match and we never need to modify
            // the Login content. Tenant identity rides in `metas.token`.
            let content: LoginContent = serde_json::from_value(req.content).map_err(|e| {
                tracing::warn!(error = %e, "frps Login: invalid request content");
                (
                    StatusCode::BAD_REQUEST,
                    Json(PluginResponse::deny("invalid login content")),
                )
            })?;

            let client_ts: i64 = content.timestamp.parse().unwrap_or(0);
            let now_ts = chrono::Utc::now().timestamp();
            if (now_ts - client_ts).abs() > LOGIN_TIMESTAMP_MAX_DRIFT_SECS {
                tracing::warn!(
                    drift = now_ts - client_ts,
                    "frps Login: timestamp drift exceeds limit"
                );
                return Err((
                    StatusCode::OK,
                    Json(PluginResponse::deny("timestamp too far from server time")),
                ));
            }

            let Some(client_token) = content.metas.get("token") else {
                tracing::warn!("frps Login: missing metadatas.token");
                return Err((
                    StatusCode::OK,
                    Json(PluginResponse::deny("authentication failed")),
                ));
            };
            if client_token.is_empty() {
                tracing::warn!("frps Login: empty metadatas.token");
                return Err((
                    StatusCode::OK,
                    Json(PluginResponse::deny("authentication failed")),
                ));
            }

            let tenants = store.list().map_err(|e| {
                tracing::error!(error = %e, "frps Login: failed to list tenants");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(PluginResponse::deny("internal error")),
                )
            })?;

            let client_token_bytes = client_token.as_bytes();
            for tenant in &tenants {
                if tenant.tunnel_token.is_empty() {
                    continue;
                }
                if tenant
                    .tunnel_token
                    .as_bytes()
                    .ct_eq(client_token_bytes)
                    .into()
                {
                    tracing::info!(
                        tenant = %tenant.id,
                        run_id = %content.run_id,
                        "frps Login: authenticated"
                    );
                    // First Login has no run ID yet. Keep nonempty IDs for
                    // diagnostics only; NewProxy never trusts this cache.
                    if !content.run_id.is_empty() {
                        state.run_id_cache.insert(
                            content.run_id.clone(),
                            tenant.id.clone(),
                            RUN_ID_CACHE_TTL,
                        );
                    }
                    return Ok(Json(PluginResponse::allow()));
                }
            }

            tracing::warn!("frps Login: no tenant matched metadatas.token");
            Err((
                StatusCode::OK,
                Json(PluginResponse::deny("authentication failed")),
            ))
        }

        "NewProxy" => {
            let content: NewProxyContent = serde_json::from_value(req.content).map_err(|e| {
                tracing::warn!(error = %e, "frps NewProxy: invalid request content");
                (
                    StatusCode::BAD_REQUEST,
                    Json(PluginResponse::deny("invalid proxy content")),
                )
            })?;

            // frps supplies the authenticated control session's Login metadata
            // in `user`. A first Login has no run ID, caches expire, and tokens
            // can be revoked while a control connection remains alive.
            let client_token = content
                .user
                .as_ref()
                .and_then(|user| user.metas.get("token"))
                .filter(|token| !token.is_empty());
            let Some(client_token) = client_token else {
                return Ok(Json(PluginResponse::deny("authentication failed")));
            };
            let tenants = store.list().map_err(|error| {
                tracing::error!(%error, "frps NewProxy: tenant store error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(PluginResponse::deny("internal error")),
                )
            })?;
            let Some(tenant) = tenants.iter().find(|tenant| {
                !tenant.tunnel_token.is_empty()
                    && bool::from(
                        tenant
                            .tunnel_token
                            .as_bytes()
                            .ct_eq(client_token.as_bytes()),
                    )
            }) else {
                return Ok(Json(PluginResponse::deny("authentication failed")));
            };

            let (authorized, refusal) = match content.proxy_type.as_str() {
                "http" | "https" => {
                    let tunnel_domain = state.tunnel_domain.as_deref().unwrap_or("octos-cloud.org");
                    let owned_domain = format!("{}.{tunnel_domain}", tenant.subdomain);
                    (
                        content.subdomain.is_empty()
                            && !content.custom_domains.is_empty()
                            && content
                                .custom_domains
                                .iter()
                                .all(|domain| domain == &owned_domain),
                        "subdomain not authorized".to_string(),
                    )
                }
                "tcp" => (
                    content.remote_port != 0 && content.remote_port == tenant.ssh_port,
                    format!("remote port {} not authorized", content.remote_port),
                ),
                _ => (false, "proxy type not authorized".to_string()),
            };
            if !authorized {
                return Ok(Json(PluginResponse::deny(refusal)));
            }
            tracing::info!(tenant = %tenant.id, proxy_type = %content.proxy_type, "frps NewProxy: authorized");
            Ok(Json(PluginResponse::allow()))
        }

        // Allow all other operations (Ping, CloseProxy, etc.)
        _ => Ok(Json(PluginResponse::allow())),
    }
}

#[cfg(test)]
mod tests {
    use crate::api::admin::{CreateTenantRequest, create_tenant};
    use crate::api::{AppState, build_router};
    use crate::config::DeploymentMode;
    use crate::tenant::{TenantConfig, TenantStatus, TenantStore};
    use axum::Json;
    use axum::extract::State;
    use axum::http::StatusCode;
    use chrono::Utc;
    use serde_json::{Value, json};
    use std::net::SocketAddr;
    use std::sync::Arc;

    /// Build a Login content payload in the shape frps sends. The per-tenant
    /// secret rides in `metas.token`; `privilege_key` and `auth.token` are
    /// empty on both sides, so frps's built-in VerifyLogin is a trivial match.
    fn login_content(tenant_token: &str, run_id: &str, timestamp: i64) -> Value {
        json!({
            "version": "0.65.0",
            "privilege_key": "",
            "timestamp": timestamp,
            "run_id": run_id,
            "metas": { "token": tenant_token },
        })
    }

    fn test_state(dir: &tempfile::TempDir) -> Arc<AppState> {
        Arc::new(AppState {
            admin_token_store: Arc::new(crate::admin_token_store::AdminTokenStore::new(dir.path())),
            setup_state_store: Arc::new(crate::setup_state_store::SetupStateStore::new(dir.path())),
            tenant_store: Some(Arc::new(TenantStore::open(dir.path()).unwrap())),
            tunnel_domain: Some("octos-cloud.org".into()),
            base_domain: None,
            frps_server: Some("127.0.0.1".into()),
            frps_port: Some(7000),
            deployment_mode: DeploymentMode::Cloud,
            ..AppState::empty_for_tests()
        })
    }

    fn save_tenant(store: &TenantStore, subdomain: &str, token: &str, ssh_port: u16) {
        let now = Utc::now();
        store
            .save(&TenantConfig {
                id: subdomain.into(),
                name: subdomain.into(),
                subdomain: subdomain.into(),
                tunnel_token: token.into(),
                ssh_port,
                local_port: 8080,
                auth_token: format!("auth-{subdomain}"),
                owner: String::new(),
                status: TenantStatus::Pending,
                created_at: now,
                updated_at: now,
            })
            .unwrap();
    }

    async fn spawn_test_server(state: Arc<AppState>) -> (String, tokio::task::JoinHandle<()>) {
        let app = build_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        tokio::task::yield_now().await;
        (format!("http://{addr}"), handle)
    }

    async fn post_plugin(base_url: &str, body: Value) -> (StatusCode, Value) {
        let response = reqwest::Client::new()
            .post(format!("{base_url}/api/internal/frps-auth"))
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let json = response.json::<Value>().await.unwrap();
        (status, json)
    }

    #[tokio::test]
    async fn should_reject_login_missing_metadatas_token() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(&dir);
        let store = state.tenant_store.as_ref().unwrap().clone();
        save_tenant(&store, "alice", "token-alice", 6001);

        let (base_url, server) = spawn_test_server(state).await;

        let (login_status, login_body) = post_plugin(
            &base_url,
            json!({
                "op": "Login",
                "content": {
                    "timestamp": Utc::now().timestamp(),
                    "run_id": "run-no-metas",
                    "version": "0.65.0"
                }
            }),
        )
        .await;
        assert_eq!(login_status, StatusCode::OK);
        assert!(login_body["reject"].as_bool().unwrap());

        server.abort();
    }

    #[tokio::test]
    async fn should_authorize_newproxy_with_valid_user_metadata_without_cached_login() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(&dir);
        let store = state.tenant_store.as_ref().unwrap().clone();
        save_tenant(&store, "alice", "token-alice", 6001);

        let (base_url, server) = spawn_test_server(state).await;

        let (http_status, http_body) = post_plugin(
            &base_url,
            json!({
                "op": "NewProxy",
                "content": {
                    "user": {"run_id": "assigned-by-frps", "metas": {"token": "token-alice"}},
                    "proxy_type": "http",
                    "custom_domains": ["alice.octos-cloud.org"]
                }
            }),
        )
        .await;
        assert_eq!(http_status, StatusCode::OK);
        assert_eq!(http_body, json!({"reject": false, "unchange": true}));

        let (tcp_status, tcp_body) = post_plugin(
            &base_url,
            json!({
                "op": "NewProxy",
                "content": {
                    "user": {"run_id": "assigned-by-frps", "metas": {"token": "token-alice"}},
                    "proxy_type": "tcp",
                    "remote_port": 6001
                }
            }),
        )
        .await;
        assert_eq!(tcp_status, StatusCode::OK);
        assert_eq!(tcp_body, json!({"reject": false, "unchange": true}));

        server.abort();
    }

    #[tokio::test]
    async fn should_reject_newproxy_for_unknown_subdomain_and_wrong_ssh_port() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(&dir);
        let store = state.tenant_store.as_ref().unwrap().clone();
        save_tenant(&store, "alice", "token-alice", 6001);

        let (base_url, server) = spawn_test_server(state).await;

        let (domain_status, domain_body) = post_plugin(
            &base_url,
            json!({
                "op": "NewProxy",
                "content": {
                    "user": {"run_id": "assigned-by-frps", "metas": {"token": "token-alice"}},
                    "proxy_type": "http",
                    "custom_domains": ["mallory.octos-cloud.org"]
                }
            }),
        )
        .await;
        assert_eq!(domain_status, StatusCode::OK);
        assert_eq!(
            domain_body,
            json!({
                "reject": true,
                "reject_reason": "subdomain not authorized",
                "unchange": false
            })
        );

        let (port_status, port_body) = post_plugin(
            &base_url,
            json!({
                "op": "NewProxy",
                "content": {
                    "user": {"run_id": "assigned-by-frps", "metas": {"token": "token-alice"}},
                    "proxy_type": "tcp",
                    "remote_port": 6002
                }
            }),
        )
        .await;
        assert_eq!(port_status, StatusCode::OK);
        assert_eq!(
            port_body,
            json!({
                "reject": true,
                "reject_reason": "remote port 6002 not authorized",
                "unchange": false
            })
        );

        server.abort();
    }

    #[tokio::test]
    async fn should_enforce_newproxy_by_subdomain_and_ssh_port_for_created_tenant() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(&dir);

        let tenant = create_tenant(
            State(state.clone()),
            Json(CreateTenantRequest {
                name: "alice".into(),
                local_port: 9090,
            }),
        )
        .await
        .unwrap()
        .0;

        let saved = state
            .tenant_store
            .as_ref()
            .unwrap()
            .get("alice")
            .unwrap()
            .unwrap();
        assert_eq!(saved.subdomain, tenant.subdomain);
        assert_eq!(saved.ssh_port, tenant.ssh_port);
        assert!(
            !saved.tunnel_token.is_empty(),
            "create_tenant should generate tunnel_token"
        );

        let (base_url, server) = spawn_test_server(state).await;

        let timestamp = Utc::now().timestamp();

        let (login_status, login_body) = post_plugin(
            &base_url,
            json!({
                "op": "Login",
                "content": login_content(&saved.tunnel_token, "run-created-alice", timestamp),
            }),
        )
        .await;
        assert_eq!(login_status, StatusCode::OK);
        assert_eq!(login_body, json!({"reject": false, "unchange": true}));

        let (proxy_status, proxy_body) = post_plugin(
            &base_url,
            json!({
                "op": "NewProxy",
                "content": {
                    "run_id": "run-created-alice",
                    "user": {"run_id": "assigned-by-frps", "metas": {"token": saved.tunnel_token}},
                    "proxy_type": "http",
                    "custom_domains": [format!("{}.octos-cloud.org", tenant.subdomain)]
                }
            }),
        )
        .await;
        assert_eq!(proxy_status, StatusCode::OK);
        assert_eq!(proxy_body, json!({"reject": false, "unchange": true}));

        let (ssh_status, ssh_body) = post_plugin(
            &base_url,
            json!({
                "op": "NewProxy",
                "content": {
                    "run_id": "run-created-alice",
                    "user": {"run_id": "assigned-by-frps", "metas": {"token": saved.tunnel_token}},
                    "proxy_type": "tcp",
                    "remote_port": tenant.ssh_port
                }
            }),
        )
        .await;
        assert_eq!(ssh_status, StatusCode::OK);
        assert_eq!(ssh_body, json!({"reject": false, "unchange": true}));

        server.abort();
    }

    // ── Per-tenant token (v2) tests ─────────────────────────────────

    #[tokio::test]
    async fn should_allow_login_when_metadatas_token_matches_tenant() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(&dir);
        let store = state.tenant_store.as_ref().unwrap().clone();
        save_tenant(&store, "alice", "per-tenant-secret-alice", 6001);

        let (base_url, server) = spawn_test_server(state).await;

        let timestamp = Utc::now().timestamp();
        let (status, body) = post_plugin(
            &base_url,
            json!({
                "op": "Login",
                "content": login_content("per-tenant-secret-alice", "run-alice-001", timestamp),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // Plugin must return unchange=true so frps uses the original Login
        // content — this is the pattern used by production frp auth plugins
        // (frp-auth-plugin). Attempting to rewrite content (unchange=false)
        // works for VerifyLogin but breaks the control session post-login.
        assert_eq!(body, json!({"reject": false, "unchange": true}));

        server.abort();
    }

    #[tokio::test]
    async fn should_deny_login_when_metadatas_token_matches_no_tenant() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(&dir);
        let store = state.tenant_store.as_ref().unwrap().clone();
        save_tenant(&store, "alice", "per-tenant-secret-alice", 6001);

        let (base_url, server) = spawn_test_server(state).await;

        let timestamp = Utc::now().timestamp();
        let (status, body) = post_plugin(
            &base_url,
            json!({
                "op": "Login",
                "content": login_content("wrong-token", "run-mallory-001", timestamp),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body["reject"].as_bool().unwrap(),
            "should reject unknown metadatas.token"
        );

        server.abort();
    }

    #[tokio::test]
    async fn should_deny_login_when_timestamp_is_stale() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(&dir);
        let store = state.tenant_store.as_ref().unwrap().clone();
        save_tenant(&store, "alice", "per-tenant-secret-alice", 6001);

        let (base_url, server) = spawn_test_server(state).await;

        let stale_timestamp = Utc::now().timestamp() - 16 * 60;
        let (status, body) = post_plugin(
            &base_url,
            json!({
                "op": "Login",
                "content": login_content(
                    "per-tenant-secret-alice",
                    "run-replay-001",
                    stale_timestamp,
                ),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body["reject"].as_bool().unwrap(),
            "should reject stale timestamp"
        );

        server.abort();
    }

    #[tokio::test]
    async fn should_authorize_newproxy_with_user_metadata_after_login() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(&dir);
        let store = state.tenant_store.as_ref().unwrap().clone();
        save_tenant(&store, "alice", "per-tenant-secret-alice", 6001);

        let (base_url, server) = spawn_test_server(state).await;

        let timestamp = Utc::now().timestamp();

        // Login first; NewProxy still authenticates its own user envelope.
        let (login_status, _) = post_plugin(
            &base_url,
            json!({
                "op": "Login",
                "content": login_content("per-tenant-secret-alice", "run-alice-002", timestamp),
            }),
        )
        .await;
        assert_eq!(login_status, StatusCode::OK);

        // NewProxy for alice's subdomain — should pass
        let (proxy_status, proxy_body) = post_plugin(
            &base_url,
            json!({
                "op": "NewProxy",
                "content": {
                    "run_id": "run-alice-002",
                    "user": {"run_id": "assigned-by-frps", "metas": {"token": "per-tenant-secret-alice"}},
                    "proxy_type": "http",
                    "custom_domains": ["alice.octos-cloud.org"]
                }
            }),
        )
        .await;
        assert_eq!(proxy_status, StatusCode::OK);
        assert_eq!(proxy_body, json!({"reject": false, "unchange": true}));

        server.abort();
    }

    #[tokio::test]
    async fn should_deny_newproxy_when_user_tenant_does_not_own_resource() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(&dir);
        let store = state.tenant_store.as_ref().unwrap().clone();
        save_tenant(&store, "alice", "per-tenant-secret-alice", 6001);
        save_tenant(&store, "bob", "per-tenant-secret-bob", 6002);

        let (base_url, server) = spawn_test_server(state).await;

        let timestamp = Utc::now().timestamp();

        // Login as alice
        post_plugin(
            &base_url,
            json!({
                "op": "Login",
                "content": login_content("per-tenant-secret-alice", "run-alice-003", timestamp),
            }),
        )
        .await;

        // Alice's authenticated user metadata cannot claim Bob's subdomain.
        let (proxy_status, proxy_body) = post_plugin(
            &base_url,
            json!({
                "op": "NewProxy",
                "content": {
                    "run_id": "run-alice-003",
                    "user": {"run_id": "assigned-by-frps", "metas": {"token": "per-tenant-secret-alice"}},
                    "proxy_type": "http",
                    "custom_domains": ["bob.octos-cloud.org"]
                }
            }),
        )
        .await;
        assert_eq!(proxy_status, StatusCode::OK);
        assert!(
            proxy_body["reject"].as_bool().unwrap(),
            "alice should not claim bob's subdomain"
        );

        server.abort();
    }

    #[tokio::test]
    async fn should_bind_each_proxy_to_current_user_credentials_after_interleaved_first_logins() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(&dir);
        let store = state.tenant_store.as_ref().unwrap().clone();
        save_tenant(&store, "alice", "token-alice", 6001);
        save_tenant(&store, "bob", "token-bob", 6002);
        let (base_url, server) = spawn_test_server(state.clone()).await;
        for token in ["token-alice", "token-bob"] {
            let (_, body) = post_plugin(
                &base_url,
                json!({
                    "op": "Login", "content": login_content(token, "", Utc::now().timestamp())
                }),
            )
            .await;
            assert_eq!(body["reject"], false);
        }
        for run_id in ["fresh", "reconnected", "no-cache-after-restart"] {
            for (domain, rejected) in [
                ("alice.octos-cloud.org", false),
                ("bob.octos-cloud.org", true),
            ] {
                let (_, body) = post_plugin(
                    &base_url,
                    json!({"op":"NewProxy", "content":{
                        "user":{"run_id":run_id,"metas":{"token":"token-alice"}},
                        "proxy_type":"http", "custom_domains":[domain]
                    }}),
                )
                .await;
                assert_eq!(body["reject"], rejected, "{run_id}: {domain}: {body}");
            }
        }
        // Rotation/revocation is checked even for an existing FRP control session.
        save_tenant(&store, "alice", "rotated-token-alice", 6001);
        let (_, body) = post_plugin(
            &base_url,
            json!({"op":"NewProxy", "content":{
                "user":{"run_id":"fresh","metas":{"token":"token-alice"}},
                "proxy_type":"tcp", "remote_port":6001
            }}),
        )
        .await;
        assert_eq!(body["reject"], true);
        server.abort();
    }

    #[tokio::test]
    async fn should_reject_proxy_without_valid_user_metadata_even_with_cached_run_id() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(&dir);
        save_tenant(
            state.tenant_store.as_ref().unwrap(),
            "alice",
            "token-alice",
            6001,
        );
        state.run_id_cache.insert(
            "cached".into(),
            "alice".into(),
            std::time::Duration::from_secs(60),
        );
        let (base_url, server) = spawn_test_server(state).await;
        for user in [
            json!(null),
            json!({"run_id":"cached"}),
            json!({"metas":{"token":""}}),
            json!({"metas":{"token":"unknown"}}),
        ] {
            let (_, body) = post_plugin(&base_url, json!({"op":"NewProxy", "content":{
                "run_id":"cached", "user":user, "proxy_type":"http", "custom_domains":["alice.octos-cloud.org"]
            }})).await;
            assert_eq!(body["reject"], true, "{user}: {body}");
        }
        server.abort();
    }

    #[tokio::test]
    async fn should_authorize_all_domains_and_only_supported_proxy_types() {
        let dir = tempfile::tempdir().unwrap();
        let state = test_state(&dir);
        let store = state.tenant_store.as_ref().unwrap();
        save_tenant(store, "alice", "token-alice", 6001);
        save_tenant(store, "bob", "token-bob", 6002);
        let (base_url, server) = spawn_test_server(state).await;
        for kind in ["http", "https"] {
            for (domains, subdomain, rejected) in [
                (json!(["alice.octos-cloud.org"]), "", false),
                (
                    json!(["alice.octos-cloud.org", "bob.octos-cloud.org"]),
                    "",
                    true,
                ),
                (
                    json!(["alice.octos-cloud.org", "outside.invalid"]),
                    "",
                    true,
                ),
                (json!([]), "", true),
                (json!(["alice.octos-cloud.org"]), "bob", true),
            ] {
                let (_, body) = post_plugin(
                    &base_url,
                    json!({"op":"NewProxy", "content":{
                        "user":{"run_id":"new","metas":{"token":"token-alice"}},
                        "proxy_type":kind, "custom_domains":domains, "subdomain":subdomain
                    }}),
                )
                .await;
                assert_eq!(
                    body["reject"], rejected,
                    "{kind} {domains} {subdomain}: {body}"
                );
            }
        }
        for (kind, port, rejected) in [
            ("tcp", 6001, false),
            ("tcp", 6002, true),
            ("udp", 6001, true),
            ("stcp", 0, true),
            ("unknown", 0, true),
        ] {
            let (_, body) = post_plugin(
                &base_url,
                json!({"op":"NewProxy", "content":{
                    "user":{"metas":{"token":"token-alice"}}, "proxy_type":kind, "remote_port":port
                }}),
            )
            .await;
            assert_eq!(body["reject"], rejected, "{kind} {port}: {body}");
        }
        server.abort();
    }
}
