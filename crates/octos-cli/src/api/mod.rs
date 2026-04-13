//! REST API and SSE streaming for octos.
//!
//! Feature-gated behind `api`. Start with `octos serve [--port 8080]`.

pub mod admin;
pub mod auth_handlers;
mod frps_plugin;
mod handlers;
pub mod metrics;
pub mod purge;
mod router;
mod sse;
mod static_files;
pub mod user_admin;
pub mod webhook_proxy;

pub use metrics::init_metrics;
pub use router::build_router;
pub use sse::SseBroadcaster;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::content_catalog::ContentCatalogManager;
use crate::login_allowlist::LoginAllowlistStore;
use crate::otp::AuthManager;
use crate::process_manager::ProcessManager;
use crate::profiles::ProfileStore;
use crate::tenant::TenantStore;
use crate::user_store::UserStore;

/// Cached mapping from frps `run_id` to the authenticated tenant ID.
///
/// Populated during Login verification and consulted during NewProxy to
/// ensure a client can only claim resources belonging to the tenant that
/// authenticated.
#[derive(Default)]
pub struct RunIdCache {
    entries: RwLock<HashMap<String, RunIdEntry>>,
}

struct RunIdEntry {
    tenant_id: String,
    expires_at: Instant,
}

impl RunIdCache {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
        }
    }

    pub fn insert(&self, run_id: String, tenant_id: String, ttl: std::time::Duration) {
        let mut map = self.entries.write().unwrap();
        map.insert(
            run_id,
            RunIdEntry {
                tenant_id,
                expires_at: Instant::now() + ttl,
            },
        );
    }

    pub fn get_tenant(&self, run_id: &str) -> Option<String> {
        let map = self.entries.read().unwrap();
        map.get(run_id).and_then(|entry| {
            if Instant::now() < entry.expires_at {
                Some(entry.tenant_id.clone())
            } else {
                None
            }
        })
    }
}

/// Shared application state for API handlers.
pub struct AppState {
    /// Agent for processing messages (None if no LLM provider configured).
    pub agent: Option<Arc<octos_agent::Agent>>,
    /// Session manager for history.
    pub sessions: Option<Arc<tokio::sync::Mutex<octos_bus::SessionManager>>>,
    /// SSE broadcaster for streaming events.
    pub broadcaster: Arc<SseBroadcaster>,
    /// Server start time.
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// Auth token (if configured).
    pub auth_token: Option<String>,
    /// Prometheus metrics handle.
    pub metrics_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
    /// Profile store for admin dashboard.
    pub profile_store: Option<Arc<ProfileStore>>,
    /// Process manager for gateway lifecycle.
    pub process_manager: Option<Arc<ProcessManager>>,
    /// User store for multi-user management.
    pub user_store: Option<Arc<UserStore>>,
    /// Allowlist for pre-authorized email-based signup.
    pub allowlist_store: Option<Arc<LoginAllowlistStore>>,
    /// Auth manager for email OTP and sessions.
    pub auth_manager: Option<Arc<AuthManager>>,
    /// Shared HTTP client for webhook proxying.
    pub http_client: reqwest::Client,
    /// Path to the global config.json file (for admin bot config editing).
    pub config_path: Option<PathBuf>,
    /// Monitor watchdog flag (shared with Monitor task).
    pub watchdog_enabled: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Monitor alerts flag (shared with Monitor task).
    pub alerts_enabled: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Persistent sysinfo instance for accurate CPU metrics across polls.
    pub sysinfo: tokio::sync::Mutex<sysinfo::System>,
    /// Tenant store for tunnel management.
    pub tenant_store: Option<Arc<TenantStore>>,
    /// Cache of frps run_id → tenant_id from Login verification.
    pub run_id_cache: Arc<RunIdCache>,
    /// Tunnel domain (e.g. "octos-cloud.org").
    pub tunnel_domain: Option<String>,
    /// frps server address for tunnel config generation.
    pub frps_server: Option<String>,
    /// frps control port.
    pub frps_port: Option<u16>,
    /// Deployment mode (local, tenant, or cloud).
    pub deployment_mode: crate::config::DeploymentMode,
    /// Whether the admin shell endpoint is enabled (default: false).
    pub allow_admin_shell: bool,
    /// Content catalog manager for per-profile file indexing.
    pub content_catalog_mgr: Option<Arc<ContentCatalogManager>>,
}

#[cfg(test)]
impl AppState {
    /// Empty `AppState` for unit tests — every store/service is `None`.
    ///
    /// Override individual fields with struct-update syntax:
    ///
    /// ```ignore
    /// let state = AppState {
    ///     profile_store: Some(profile_store),
    ///     ..AppState::empty_for_tests()
    /// };
    /// ```
    pub(crate) fn empty_for_tests() -> Self {
        Self {
            agent: None,
            sessions: None,
            broadcaster: Arc::new(SseBroadcaster::new(16)),
            started_at: chrono::Utc::now(),
            auth_token: None,
            metrics_handle: None,
            profile_store: None,
            process_manager: None,
            user_store: None,
            allowlist_store: None,
            auth_manager: None,
            http_client: reqwest::Client::new(),
            config_path: None,
            watchdog_enabled: None,
            alerts_enabled: None,
            sysinfo: tokio::sync::Mutex::new(sysinfo::System::new()),
            tenant_store: None,
            run_id_cache: Arc::new(RunIdCache::new()),
            tunnel_domain: None,
            frps_server: None,
            frps_port: None,
            deployment_mode: crate::config::DeploymentMode::Local,
            allow_admin_shell: false,
            content_catalog_mgr: None,
        }
    }
}
