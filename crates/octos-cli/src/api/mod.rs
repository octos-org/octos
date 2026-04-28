//! REST API and SSE streaming for octos.
//!
//! Feature-gated behind `api`. Start with `octos serve [--port 50080]`.

pub mod admin;
pub mod admin_setup;
pub mod auth_handlers;
mod events_harness;
mod frps_plugin;
mod handlers;
pub mod metrics;
pub mod purge;
mod router;
mod sse;
mod static_files;
pub mod swarm;
pub mod user_admin;
pub mod webhook_proxy;

pub use metrics::init_metrics;
pub use router::{DEFAULT_BASE_DOMAIN, build_router, cors_allowlist_for_base_domain};
pub use sse::SseBroadcaster;
pub use swarm::{
    BroadcasterSwarmEventSink, CostAttributionView, CostAttributionsResponse, DispatchIndexRow,
    SubtaskView, SwarmBudgetSpec, SwarmContextSpec, SwarmDispatchDetail, SwarmDispatchRequest,
    SwarmDispatchResponse, SwarmDispatchesResponse, SwarmReviewRequest, SwarmReviewResponse,
    SwarmState, TestStubBackend, ValidatorView, build_swarm_state, build_test_swarm_state,
    build_test_swarm_state_with_broadcaster, parallel_topology,
};

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::admin_token_store::AdminTokenStore;
use crate::content_catalog::ContentCatalogManager;
use crate::login_allowlist::LoginAllowlistStore;
use crate::otp::AuthManager;
use crate::process_manager::ProcessManager;
use crate::profiles::ProfileStore;
use crate::setup_state_store::SetupStateStore;
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
    /// Bootstrap admin auth token from config/env (used only until the
    /// hashed admin-token file is created via dashboard rotation).
    pub auth_token: Option<String>,
    /// Hashed admin token store at `{data_dir}/admin_token.json`.
    /// When present, authoritative for admin auth — the bootstrap token is
    /// ignored until the file is cleared via `octos admin reset-token`.
    pub admin_token_store: Arc<AdminTokenStore>,
    /// Setup-wizard state store at `{data_dir}/setup_state.json`.
    /// Tracks wizard completion, skip status, and last step reached so the
    /// dashboard can gate and resume the first-run flow.
    pub setup_state_store: Arc<SetupStateStore>,
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
    /// Public-facing base domain each mini serves profiles under
    /// (e.g. `"crew.ominix.io"`, `"bot.ominix.io"`, `"ocean.ominix.io"`).
    /// `None` is treated as `"crew.ominix.io"` by callers for backward
    /// compatibility. See `crate::config::Config::base_domain` for the
    /// config / env-var wiring.
    pub base_domain: Option<String>,
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
    /// Shared swarm state for the M7.6 contract-authoring dashboard.
    /// `None` when swarm wiring is not configured — handlers return
    /// `503 Service Unavailable` in that case.
    pub swarm_state: Option<Arc<swarm::SwarmState>>,
    /// Optional path to the JSONL harness-event sink. When `Some`,
    /// typed harness events (e.g. `SwarmReviewDecision`) are appended
    /// to the file in addition to being broadcast live to SSE
    /// subscribers. When `None`, events are broadcast-only — so a
    /// decision made while no subscriber is connected is lost. Wired
    /// by `octos serve` from the `OCTOS_HARNESS_EVENT_SINK` env var.
    pub harness_event_sink_path: Option<String>,
    /// Credential pool (M6.5, F-005). Initialised at startup from
    /// `config.credential_pool` when present; `None` falls back to the
    /// legacy single-credential flow. Shared with session actors so
    /// per-LLM-call `acquire`/`mark_*` operations see a consistent view.
    pub credential_pool: Option<Arc<octos_llm::PersistentCredentialPool>>,
    /// Content classifier (M6.6, F-005). Populated when
    /// `config.content_routing` is present and `enabled: true`. When
    /// `None` the router falls through to the unclassified strong-only
    /// default (invariant #3 of the M6.6 spec).
    pub content_classifier: Option<Arc<octos_llm::ContentClassifier>>,
    /// M7.9 / W2: shared session-task supervisor lookup. Used by the
    /// `POST /api/tasks/{task_id}/cancel` and
    /// `POST /api/tasks/{task_id}/restart-from-node` endpoints to
    /// forward to the matching `TaskSupervisor`. `None` keeps the
    /// pre-W2 behaviour — both endpoints return `503 Service
    /// Unavailable` so they fail closed instead of pretending a task
    /// was cancelled.
    pub task_query_store: Option<crate::session_actor::SessionTaskQueryStore>,
}

impl AppState {
    /// Empty `AppState` for unit and integration tests — every
    /// store/service is `None`.
    ///
    /// Override individual fields with struct-update syntax:
    ///
    /// ```ignore
    /// let state = AppState {
    ///     profile_store: Some(profile_store),
    ///     ..AppState::empty_for_tests()
    /// };
    /// ```
    pub fn empty_for_tests() -> Self {
        let tmp =
            std::env::temp_dir().join(format!("octos-test-admin-token-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).ok();
        Self {
            agent: None,
            sessions: None,
            broadcaster: Arc::new(SseBroadcaster::new(16)),
            started_at: chrono::Utc::now(),
            auth_token: None,
            admin_token_store: Arc::new(AdminTokenStore::new(&tmp)),
            setup_state_store: Arc::new(SetupStateStore::new(&tmp)),
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
            base_domain: None,
            frps_server: None,
            frps_port: None,
            deployment_mode: crate::config::DeploymentMode::Local,
            allow_admin_shell: false,
            content_catalog_mgr: None,
            swarm_state: None,
            harness_event_sink_path: None,
            credential_pool: None,
            content_classifier: None,
            task_query_store: None,
        }
    }
}
