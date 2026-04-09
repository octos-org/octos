//! frps server plugin handler for shared-token tunnel authorization.
//!
//! frps sends HTTP requests to this endpoint for Login and NewProxy operations.
//! Login is allowed with the host's shared FRPS token, and NewProxy is checked
//! against the tenant store to ensure the requested subdomain or SSH port
//! belongs to a registered tenant.
//!
//! Protocol: <https://github.com/fatedier/frp/blob/dev/doc/server_plugin.md>

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Json;
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use super::AppState;

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

/// NewProxy content from frps — contains the proxy configuration being registered.
#[derive(Debug, Deserialize)]
struct NewProxyContent {
    /// The shared auth token from the original login.
    #[serde(default)]
    privilege_key: String,
    /// Proxy type (http, tcp, etc.).
    #[serde(default)]
    proxy_type: String,
    /// Custom domains for HTTP proxies.
    #[serde(default)]
    custom_domains: Vec<String>,
    /// Remote port for TCP proxies.
    #[serde(default)]
    remote_port: u16,
}

/// POST /api/internal/frps-auth — frps server plugin handler.
///
/// Handles two operations:
/// - `Login`: Allows frps clients that already passed shared-token auth.
/// - `NewProxy`: Ensures the requested subdomain or SSH port is assigned.
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
        "Login" => Ok(Json(PluginResponse::allow())),

        "NewProxy" => {
            let content: NewProxyContent = serde_json::from_value(req.content).map_err(|e| {
                tracing::warn!(error = %e, "frps NewProxy: invalid request content");
                (
                    StatusCode::BAD_REQUEST,
                    Json(PluginResponse::deny("invalid proxy content")),
                )
            })?;

            if content.proxy_type == "http" {
                let tunnel_domain = state.tunnel_domain.as_deref().unwrap_or("octos-cloud.org");
                let Some(requested_domain) = content.custom_domains.first() else {
                    tracing::warn!("frps NewProxy: missing custom domain");
                    return Err((
                        StatusCode::OK,
                        Json(PluginResponse::deny("subdomain not authorized")),
                    ));
                };
                let suffix = format!(".{tunnel_domain}");
                let Some(subdomain) = requested_domain.strip_suffix(&suffix) else {
                    tracing::warn!(
                        requested = ?content.custom_domains,
                        "frps NewProxy: subdomain mismatch"
                    );
                    return Err((
                        StatusCode::OK,
                        Json(PluginResponse::deny("subdomain not authorized")),
                    ));
                };

                match store.find_by_subdomain(subdomain) {
                    Ok(Some(tenant)) => {
                        tracing::info!(
                            tenant = %tenant.id,
                            proxy_type = %content.proxy_type,
                            "frps NewProxy: authorized"
                        );
                        return Ok(Json(PluginResponse::allow()));
                    }
                    Ok(None) => {
                        tracing::warn!(
                            requested = %requested_domain,
                            "frps NewProxy: subdomain not assigned"
                        );
                        return Err((
                            StatusCode::OK,
                            Json(PluginResponse::deny("subdomain not authorized")),
                        ));
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "frps NewProxy: tenant store error");
                        return Err((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(PluginResponse::deny("internal error")),
                        ));
                    }
                }
            }

            if content.proxy_type == "tcp" {
                match store.find_by_ssh_port(content.remote_port) {
                    Ok(Some(tenant)) => {
                        tracing::info!(
                            tenant = %tenant.id,
                            proxy_type = %content.proxy_type,
                            "frps NewProxy: authorized"
                        );
                        return Ok(Json(PluginResponse::allow()));
                    }
                    Ok(None) => {
                        tracing::warn!(
                            requested_port = content.remote_port,
                            "frps NewProxy: port not authorized"
                        );
                        return Err((
                            StatusCode::OK,
                            Json(PluginResponse::deny(format!(
                                "remote port {} not authorized",
                                content.remote_port
                            ))),
                        ));
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "frps NewProxy: tenant store error");
                        return Err((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(PluginResponse::deny("internal error")),
                        ));
                    }
                }
            }

            Ok(Json(PluginResponse::allow()))
        }

        // Allow all other operations (Ping, CloseProxy, etc.)
        _ => Ok(Json(PluginResponse::allow())),
    }
}

#[cfg(test)]
mod tests {
    use crate::api::admin::{CreateTenantRequest, create_tenant};
    use crate::api::{AppState, SseBroadcaster, build_router};
    use crate::config::DeploymentMode;
    use crate::tenant::{TenantConfig, TenantStatus, TenantStore};
    use axum::Json;
    use axum::extract::State;
    use axum::http::StatusCode;
    use chrono::Utc;
    use serde_json::{Value, json};
    use std::net::SocketAddr;
    use std::sync::Arc;

    fn test_state(dir: &tempfile::TempDir) -> Arc<AppState> {
        Arc::new(AppState {
            agent: None,
            sessions: None,
            broadcaster: Arc::new(SseBroadcaster::new(16)),
            started_at: Utc::now(),
            auth_token: None,
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
            tenant_store: Some(Arc::new(TenantStore::open(dir.path()).unwrap())),
            tunnel_domain: Some("octos-cloud.org".into()),
            frps_server: Some("127.0.0.1".into()),
            frps_port: Some(7000),
            deployment_mode: DeploymentMode::Cloud,
            allow_admin_shell: false,
            content_catalog_mgr: None,
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
    async fn login_is_allowed_without_per_tenant_token_lookup() {
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
                    "privilege_key": "shared-host-token"
                }
            }),
        )
        .await;
        assert_eq!(login_status, StatusCode::OK);
        assert_eq!(login_body, json!({"reject": false, "unchange": true}));

        server.abort();
    }

    #[tokio::test]
    async fn newproxy_is_authorized_by_subdomain_and_ssh_port_in_shared_token_mode() {
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
                    "privilege_key": "shared-host-token",
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
                    "privilege_key": "shared-host-token",
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
    async fn rejects_unknown_subdomains_and_wrong_ssh_ports_even_with_shared_token() {
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
                    "privilege_key": "shared-host-token",
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
                    "privilege_key": "shared-host-token",
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
    async fn create_tenant_output_is_enforced_by_newproxy_without_using_tunnel_token() {
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

        let (base_url, server) = spawn_test_server(state).await;

        let (login_status, login_body) = post_plugin(
            &base_url,
            json!({
                "op": "Login",
                "content": {
                    "privilege_key": "shared-host-token"
                }
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
                    "privilege_key": "shared-host-token",
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
                    "privilege_key": "shared-host-token",
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
}
