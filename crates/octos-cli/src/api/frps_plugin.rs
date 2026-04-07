//! frps server plugin handler for per-tenant tunnel authentication.
//!
//! frps sends HTTP requests to this endpoint for Login and NewProxy operations.
//! We validate the tunnel_token against the tenant store and ensure subdomain
//! matches the authenticated tenant.
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

/// Login content from frps — contains the client's privilege_key (auth token).
#[derive(Debug, Deserialize)]
struct LoginContent {
    /// The auth token sent by frpc (we expect this to be the tenant's tunnel_token).
    #[serde(default)]
    privilege_key: String,
}

/// NewProxy content from frps — contains the proxy configuration being registered.
#[derive(Debug, Deserialize)]
struct NewProxyContent {
    /// The auth token from the original login.
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
/// - `Login`: Validates the tunnel_token against the tenant store.
/// - `NewProxy`: Ensures the subdomain matches the authenticated tenant.
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
            let content: LoginContent = serde_json::from_value(req.content).map_err(|e| {
                tracing::warn!(error = %e, "frps Login: invalid request content");
                (
                    StatusCode::BAD_REQUEST,
                    Json(PluginResponse::deny("invalid login content")),
                )
            })?;

            if content.privilege_key.is_empty() {
                tracing::warn!("frps Login: empty token");
                return Err((
                    StatusCode::OK,
                    Json(PluginResponse::deny("missing auth token")),
                ));
            }

            // Look up the tenant by tunnel_token
            match store.find_by_tunnel_token(&content.privilege_key) {
                Ok(Some(tenant)) => {
                    tracing::info!(
                        tenant = %tenant.id,
                        "frps Login: authenticated"
                    );
                    Ok(Json(PluginResponse::allow()))
                }
                Ok(None) => {
                    tracing::warn!("frps Login: unknown token");
                    Err((
                        StatusCode::OK,
                        Json(PluginResponse::deny("invalid auth token")),
                    ))
                }
                Err(e) => {
                    tracing::error!(error = %e, "frps Login: tenant store error");
                    Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(PluginResponse::deny("internal error")),
                    ))
                }
            }
        }

        "NewProxy" => {
            let content: NewProxyContent =
                serde_json::from_value(req.content).map_err(|e| {
                    tracing::warn!(error = %e, "frps NewProxy: invalid request content");
                    (
                        StatusCode::BAD_REQUEST,
                        Json(PluginResponse::deny("invalid proxy content")),
                    )
                })?;

            // Look up the tenant by the token used during login
            let tenant = match store.find_by_tunnel_token(&content.privilege_key) {
                Ok(Some(t)) => t,
                Ok(None) => {
                    tracing::warn!("frps NewProxy: unknown token");
                    return Err((
                        StatusCode::OK,
                        Json(PluginResponse::deny("invalid auth token")),
                    ));
                }
                Err(e) => {
                    tracing::error!(error = %e, "frps NewProxy: tenant store error");
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(PluginResponse::deny("internal error")),
                    ));
                }
            };

            // For HTTP proxies, validate the subdomain belongs to this tenant
            if content.proxy_type == "http" {
                let tunnel_domain = state.tunnel_domain.as_deref().unwrap_or("octos-cloud.org");
                let expected_domain = format!("{}.{}", tenant.subdomain, tunnel_domain);

                if content.custom_domains.is_empty()
                    || !content.custom_domains.iter().all(|d| d == &expected_domain)
                {
                    tracing::warn!(
                        tenant = %tenant.id,
                        requested = ?content.custom_domains,
                        expected = %expected_domain,
                        "frps NewProxy: subdomain mismatch"
                    );
                    return Err((
                        StatusCode::OK,
                        Json(PluginResponse::deny(format!(
                            "subdomain not authorized — expected {}",
                            expected_domain
                        ))),
                    ));
                }
            }

            // For TCP proxies, validate the remote port matches the tenant's allocated SSH port
            if content.proxy_type == "tcp" && content.remote_port != tenant.ssh_port {
                tracing::warn!(
                    tenant = %tenant.id,
                    requested_port = content.remote_port,
                    allocated_port = tenant.ssh_port,
                    "frps NewProxy: port not authorized"
                );
                return Err((
                    StatusCode::OK,
                    Json(PluginResponse::deny(format!(
                        "remote port {} not authorized — allocated port is {}",
                        content.remote_port, tenant.ssh_port
                    ))),
                ));
            }

            tracing::info!(
                tenant = %tenant.id,
                proxy_type = %content.proxy_type,
                "frps NewProxy: authorized"
            );
            Ok(Json(PluginResponse::allow()))
        }

        // Allow all other operations (Ping, CloseProxy, etc.)
        _ => Ok(Json(PluginResponse::allow())),
    }
}
