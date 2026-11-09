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
