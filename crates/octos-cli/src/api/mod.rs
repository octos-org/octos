//! REST API and SSE streaming for octos.
//!
//! Feature-gated behind `api`. Start with `octos serve [--port 8080]`.

pub mod admin;
pub mod auth_handlers;
mod handlers;
pub mod metrics;
mod router;
mod sse;
mod static_files;
pub mod user_admin;
pub mod webhook_proxy;

pub use metrics::init_metrics;
pub use router::build_router;
pub use sse::SseBroadcaster;

use std::path::PathBuf;
use std::sync::Arc;

use crate::otp::AuthManager;
use crate::process_manager::ProcessManager;
use crate::profiles::ProfileStore;
use crate::user_store::UserStore;

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
}
