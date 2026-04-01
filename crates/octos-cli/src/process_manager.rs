//! Gateway and bridge child process lifecycle management.
//!
//! Spawns `octos gateway` and optionally `node bridge.js` (WhatsApp) as child
//! processes, monitors their output, and provides start/stop/status/log-streaming
//! capabilities. Managed WhatsApp bridges are auto-spawned when a profile with
//! a WhatsApp channel (no explicit `bridge_url`) is started.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{Result, bail};
use octos_agent::sandbox::BLOCKED_ENV_VARS;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
#[cfg(feature = "api")]
use tokio::sync::mpsc;
use tokio::sync::{Mutex, RwLock, broadcast, watch};

use crate::profiles::{ChannelCredentials, ProfileStore, UserProfile};

/// Base port for managed WhatsApp bridge WebSocket servers.
/// HTTP media port = WS port + 1.
const BRIDGE_BASE_WS_PORT: u16 = 3101;

/// Base port for auto-assigned Feishu/Twilio webhook servers.
const WEBHOOK_BASE_PORT: u16 = 9321;

/// Base port for auto-assigned API channel servers (serve mode).
const API_BASE_PORT: u16 = 9401;

/// Manages gateway and bridge child processes — one of each per user profile.
pub struct ProcessManager {
    processes: Arc<RwLock<HashMap<String, GatewayProcess>>>,
    bridges: Arc<RwLock<HashMap<String, BridgeProcess>>>,
    profile_store: Arc<ProfileStore>,
    /// Path to bridge.js. If None, managed bridges are disabled.
    bridge_js_path: Option<PathBuf>,
    /// Optional channel for sending admin alerts when gateways exit.
    #[cfg(feature = "api")]
    alert_tx: std::sync::Mutex<Option<mpsc::Sender<crate::monitor::AdminAlert>>>,
    /// Port that `octos serve` is listening on (for admin mode gateways).
    serve_port: Option<u16>,
    /// Admin token for API access (passed to admin mode gateways).
    admin_token: Option<String>,
    /// Weak self-reference for auto-restart from spawned tasks.
    self_ref: std::sync::Mutex<Option<std::sync::Weak<ProcessManager>>>,
}

struct GatewayProcess {
    pid: u32,
    started_at: DateTime<Utc>,
    log_tx: broadcast::Sender<String>,
    stop_tx: watch::Sender<bool>,
    /// Feishu/Twilio webhook port this gateway is listening on (if any).
    webhook_port: Option<u16>,
    /// API channel port this gateway is listening on (if any).
    api_port: Option<u16>,
    /// Ring buffer of recent log lines so new subscribers can see history.
    log_history: Arc<Mutex<Vec<String>>>,
}

/// Max number of log lines to retain per gateway process.
const LOG_HISTORY_MAX: usize = 500;

#[allow(dead_code)]
struct BridgeProcess {
    pid: u32,
    ws_port: u16,
    http_port: u16,
    started_at: DateTime<Utc>,
    qr_code: Arc<Mutex<Option<String>>>,
    status: Arc<Mutex<BridgeStatus>>,
    phone_number: Arc<Mutex<Option<String>>>,
    lid: Arc<Mutex<Option<String>>>,
    log_tx: broadcast::Sender<String>,
    stop_tx: watch::Sender<bool>,
}

/// WhatsApp bridge connection status.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BridgeStatus {
    /// Bridge started, waiting for QR scan.
    Waiting,
    /// WhatsApp connected.
    Connected,
    /// Disconnected (may auto-reconnect).
    Disconnected,
    /// Logged out — needs re-pairing.
    LoggedOut,
}

/// Status of a gateway process.
#[derive(Debug, Clone, Serialize)]
pub struct ProcessStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub started_at: Option<String>,
    pub uptime_secs: Option<i64>,
}

/// WhatsApp bridge QR + status info returned to the dashboard.
#[derive(Debug, Clone, Serialize)]
pub struct BridgeQrInfo {
    pub qr: Option<String>,
    pub status: BridgeStatus,
    pub ws_port: u16,
    pub http_port: u16,
    /// Phone number of the connected WhatsApp account (e.g. "14088882719").
    pub phone_number: Option<String>,
    /// WhatsApp LID of the connected account (e.g. "197061790171194").
    pub lid: Option<String>,
}

impl ProcessManager {
    /// Create a new process manager backed by the given profile store.
    pub fn new(profile_store: Arc<ProfileStore>) -> Self {
        Self {
            processes: Arc::new(RwLock::new(HashMap::new())),
            bridges: Arc::new(RwLock::new(HashMap::new())),
            profile_store,
            bridge_js_path: None,
            #[cfg(feature = "api")]
            alert_tx: std::sync::Mutex::new(None),
            serve_port: None,
            admin_token: None,
            self_ref: std::sync::Mutex::new(None),
        }
    }

    /// Store a weak self-reference for auto-restart from spawned monitor tasks.
    /// Must be called after wrapping in `Arc`.
    pub fn set_self_ref(self: &Arc<Self>) {
        *self.self_ref.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::downgrade(self));
    }

    /// Set the serve port and admin token (for admin mode gateways).
    pub fn with_serve_config(mut self, port: u16, token: Option<String>) -> Self {
        self.serve_port = Some(port);
        self.admin_token = token;
        self
    }

    /// Set the alert sender for monitor notifications.
    #[cfg(feature = "api")]
    pub fn set_alert_sender(&self, tx: mpsc::Sender<crate::monitor::AdminAlert>) {
        *self.alert_tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    }

    /// Set the path to bridge.js for managed WhatsApp bridges.
    pub fn with_bridge_js(mut self, path: PathBuf) -> Self {
        if path.exists() {
            self.bridge_js_path = Some(path);
        } else {
            tracing::warn!(path = %path.display(), "bridge.js not found, managed WhatsApp bridges disabled");
        }
        self
    }

    // ── Gateway lifecycle ──────────────────────────────────────────────

    /// Start the gateway for a profile. Returns an error if already running.
    /// If the profile has a managed WhatsApp channel, the bridge is started first.
    pub async fn start(&self, profile: &UserProfile) -> Result<()> {
        // Hold the write lock for the entire operation to prevent TOCTOU races.
        tracing::debug!(profile = %profile.id, "start: acquiring processes write lock");
        let mut procs = self.processes.write().await;
        tracing::debug!(profile = %profile.id, "start: write lock acquired");
        if procs.contains_key(&profile.id) {
            bail!("gateway for '{}' is already running", profile.id);
        }

        // Check if profile needs a managed WhatsApp bridge
        let bridge_url_override = if self.needs_managed_bridge(profile) {
            match self.start_bridge_inner(profile).await {
                Ok(ws_port) => Some(format!("ws://localhost:{ws_port}")),
                Err(e) => {
                    tracing::warn!(
                        profile = %profile.id,
                        error = %e,
                        "failed to start managed WhatsApp bridge, continuing without it"
                    );
                    None
                }
            }
        } else {
            None
        };

        // Start WeChat bridge if needed
        let wechat_bridge_url = if self.needs_wechat_bridge(profile) {
            match self.start_wechat_bridge(profile).await {
                Ok(ws_port) => Some(format!("ws://localhost:{ws_port}")),
                Err(e) => {
                    tracing::warn!(
                        profile = %profile.id,
                        error = %e,
                        "failed to start WeChat bridge, continuing without it"
                    );
                    None
                }
            }
        } else {
            None
        };

        // Auto-assign Feishu webhook port if needed
        let feishu_port = match crate::profiles::feishu_webhook_port(profile) {
            Some(Some(port)) => Some(port),
            Some(None) => Some(self.allocate_webhook_port(&procs)),
            None => None,
        };

        // Detect API channel port from profile config.
        // In serve mode, auto-allocate an API port so the serve API can proxy
        // chat/session requests to the child gateway.
        let api_port = crate::profiles::api_channel_port(profile).or_else(|| {
            if self.serve_port.is_some() {
                let port = self.allocate_api_port(&procs);
                tracing::info!(
                    profile = %profile.id,
                    port,
                    "auto-allocated API port for serve mode"
                );
                Some(port)
            } else {
                None
            }
        });

        // Resolve data directory and ensure subdirs exist
        tracing::debug!(profile = %profile.id, "start: resolving data dir");
        let data_dir = self.profile_store.resolve_data_dir(profile);
        for sub in ["memory", "sessions", "research", "skills", "history"] {
            std::fs::create_dir_all(data_dir.join(sub))?;
        }

        // Spawn the gateway as a child process, pointing at the profile JSON directly
        tracing::debug!(profile = %profile.id, "start: building command");
        let exe = std::env::current_exe()?;
        let profile_path = self.profile_store.profile_path(&profile.id);
        let mut cmd = Command::new(&exe);
        cmd.arg("gateway")
            .arg("--profile")
            .arg(&profile_path)
            .arg("--data-dir")
            .arg(&data_dir)
            .arg("--cwd")
            .arg(&data_dir)
            .current_dir(&data_dir) // OS-level cwd matches logical --cwd for consistent path resolution
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        if let Some(ref url) = bridge_url_override {
            cmd.arg("--bridge-url").arg(url);
        }
        if let Some(ref url) = wechat_bridge_url {
            cmd.arg("--wechat-bridge-url").arg(url);
        }
        if let Some(port) = feishu_port {
            cmd.arg("--feishu-port").arg(port.to_string());
        }
        if let Some(port) = api_port {
            cmd.arg("--api-port").arg(port.to_string());
        }

        // Pass octos home dir so gateway can open ProfileStore for /account commands
        cmd.arg("--octos-home")
            .arg(self.profile_store.octos_home_dir());

        // Sub-account: pass parent profile path and merge parent env vars
        tracing::debug!(profile = %profile.id, "start: checking sub-account");
        if let Some(ref parent_id) = profile.parent_id {
            let parent_path = self.profile_store.profile_path(parent_id);
            cmd.arg("--parent-profile").arg(&parent_path);

            // Merge parent env_vars (API keys) into child process,
            // resolving keychain markers.
            if let Ok(Some(parent)) = self.profile_store.get(parent_id) {
                let resolved_parent =
                    crate::auth::keychain::resolve_env_vars(&parent.config.env_vars);
                for (key, value) in &resolved_parent {
                    // Sub-account's own env_vars take priority
                    if !profile.config.env_vars.contains_key(key) {
                        if !BLOCKED_ENV_VARS
                            .iter()
                            .any(|blocked| key.eq_ignore_ascii_case(blocked))
                        {
                            cmd.env(key, value);
                        }
                    }
                }
            }
        }

        // Inject OminiX API URL for all gateways (platform-wide, not per-profile)
        let ominix_url =
            std::env::var("OMINIX_API_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());
        cmd.env("OMINIX_API_URL", &ominix_url);

        // Admin mode: inject OCTOS_SERVE_URL and OCTOS_ADMIN_TOKEN
        if profile.config.admin_mode {
            if let Some(port) = self.serve_port {
                cmd.env("OCTOS_SERVE_URL", format!("http://127.0.0.1:{}", port));
            }
            if let Some(ref token) = self.admin_token {
                cmd.env("OCTOS_ADMIN_TOKEN", token);
            }
        }

        // Inject email config as env vars for the send_email plugin.
        // The dashboard sets email config in the profile JSON, but the
        // send_email app-skill reads SMTP_HOST / SMTP_PASSWORD / etc.
        // from env vars. Bridge the gap here.
        if let Some(ref email) = profile.config.email {
            for (key, value) in email.to_env_vars(&profile.config.env_vars) {
                // Don't override if already set explicitly in env_vars
                if !profile.config.env_vars.contains_key(&key) {
                    cmd.env(&key, &value);
                }
            }
        }

        // Pass env vars from profile config, resolving keychain markers and
        // filtering out dangerous ones.
        tracing::debug!(profile = %profile.id, "start: resolving env vars");
        let resolved_env_vars = crate::auth::keychain::resolve_env_vars(&profile.config.env_vars);
        for (key, value) in &resolved_env_vars {
            if BLOCKED_ENV_VARS
                .iter()
                .any(|blocked| key.eq_ignore_ascii_case(blocked))
            {
                tracing::warn!(
                    profile = %profile.id,
                    var = %key,
                    "skipping blocked environment variable"
                );
                continue;
            }
            cmd.env(key, value);
        }

        tracing::debug!(profile = %profile.id, "start: spawning gateway subprocess");
        let mut child = cmd.spawn()?;
        tracing::debug!(profile = %profile.id, "start: gateway subprocess spawned");

        let pid = child.id().unwrap_or(0);
        let (log_tx, _) = broadcast::channel::<String>(1024);
        // Subscribe before spawning readers so we capture all output for startup check.
        let startup_rx = log_tx.subscribe();
        let (stop_tx, stop_rx) = watch::channel(false);
        let log_history: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        let process = GatewayProcess {
            pid,
            started_at: Utc::now(),
            log_tx: log_tx.clone(),
            stop_tx,
            webhook_port: feishu_port,
            api_port,
            log_history: log_history.clone(),
        };

        procs.insert(profile.id.clone(), process);
        // Release the write lock now that the entry is inserted.
        drop(procs);

        // Spawn task to read stdout and forward to log channel + server console
        let has_stdout = child.stdout.is_some();
        let has_stderr = child.stderr.is_some();
        tracing::info!(
            profile = %profile.id,
            pid = pid,
            stdout = has_stdout,
            stderr = has_stderr,
            "spawned gateway, attaching log readers"
        );

        if let Some(stdout) = child.stdout.take() {
            let tx = log_tx.clone();
            let hist = log_history.clone();
            let profile_id_label = profile.id.clone();
            tokio::spawn(async move {
                tracing::debug!(profile = %profile_id_label, "stdout reader started");
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::info!(profile = %profile_id_label, "{line}");
                    let _ = tx.send(line.clone());
                    let mut buf = hist.lock().await;
                    buf.push(line);
                    if buf.len() > LOG_HISTORY_MAX {
                        buf.remove(0);
                    }
                }
                tracing::debug!(profile = %profile_id_label, "stdout reader ended");
            });
        }

        // Spawn task to read stderr and forward to log channel + server console
        if let Some(stderr) = child.stderr.take() {
            let tx = log_tx.clone();
            let hist = log_history.clone();
            let profile_id_label = profile.id.clone();
            tokio::spawn(async move {
                tracing::debug!(profile = %profile_id_label, "stderr reader started");
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::warn!(profile = %profile_id_label, "{line}");
                    let _ = tx.send(line.clone());
                    let mut buf = hist.lock().await;
                    buf.push(line);
                    if buf.len() > LOG_HISTORY_MAX {
                        buf.remove(0);
                    }
                }
                tracing::debug!(profile = %profile_id_label, "stderr reader ended");
            });
        }

        // Spawn task to wait for process exit or stop signal.
        let profile_id = profile.id.clone();
        let processes = Arc::clone(&self.processes);
        let profile_store_for_restart = Arc::clone(&self.profile_store);
        let pm_weak = self
            .self_ref
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        #[cfg(feature = "api")]
        let alert_tx = self
            .alert_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        tokio::spawn(async move {
            let mut stop_rx = stop_rx;
            tokio::select! {
                status = child.wait() => {
                    processes.write().await.remove(&profile_id);
                    let exit_code = match &status {
                        Ok(s) => {
                            tracing::info!(profile = %profile_id, exit = %s, "gateway exited");
                            s.code()
                        }
                        Err(e) => {
                            tracing::error!(profile = %profile_id, error = %e, "gateway error");
                            None
                        }
                    };
                    #[cfg(feature = "api")]
                    if let Some(ref tx) = alert_tx {
                        let _ = tx.try_send(crate::monitor::AdminAlert::GatewayExited {
                            profile_id: profile_id.clone(),
                            exit_code,
                            timestamp: Utc::now(),
                        });
                    }
                    let _ = exit_code; // suppress unused warning without admin-bot

                    // Auto-restart: if the gateway exited unexpectedly (not via
                    // stop signal), restart it after a brief delay. The Monitor
                    // (if configured) handles smarter restart logic with
                    // max-attempts; this is a basic fallback so gateways never
                    // stay down when there is no Monitor.
                    if let Some(weak) = pm_weak.clone() {
                        let pid2 = profile_id.clone();
                        let ps2 = profile_store_for_restart.clone();
                        // Use spawn_local-safe pattern: sleep then restart on current runtime
                        let rt = tokio::runtime::Handle::current();
                        std::thread::spawn(move || {
                            std::thread::sleep(std::time::Duration::from_secs(2));
                            rt.block_on(async move {
                                if let Some(pm) = weak.upgrade() {
                                    if let Ok(Some(profile)) = ps2.get(&pid2) {
                                        tracing::info!(profile = %pid2, "auto-restarting crashed gateway");
                                        if let Err(e) = pm.start(&profile).await {
                                            tracing::error!(profile = %pid2, error = %e, "auto-restart failed");
                                        }
                                    }
                                }
                            });
                        });
                    }
                }
                _ = stop_rx.changed() => {
                    let _ = child.kill().await;
                    tracing::info!(profile = %profile_id, "gateway stopped");
                }
            }
        });

        // Wait briefly to catch immediate startup failures (e.g. missing API key,
        // config errors). If the process exits within this window, report the error
        // instead of returning Ok.  We check in a loop so we detect exit quickly
        // while still giving stderr time to flush.
        let mut startup_rx = startup_rx;
        for _ in 0..10 {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let procs = self.processes.read().await;
            if !procs.contains_key(&profile.id) {
                // Process already exited and was removed by the monitor task.
                drop(procs); // release read lock
                // Give reader tasks time to flush remaining pipe data to the
                // broadcast channel before we drain it.
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                // Drain log lines captured by our early subscriber.
                let mut lines = Vec::new();
                while let Ok(line) = startup_rx.try_recv() {
                    lines.push(line);
                }
                let detail = if lines.is_empty() {
                    "gateway exited immediately (no output captured)".to_string()
                } else {
                    lines.join("\n")
                };
                tracing::error!(profile = %profile.id, "gateway failed to start:\n{detail}");
                bail!("gateway failed to start:\n{detail}");
            }
        }

        tracing::debug!(profile = %profile.id, pid = pid, "gateway started");
        Ok(())
    }

    /// Stop the gateway for a profile. Also stops managed bridge if running.
    pub async fn stop(&self, profile_id: &str) -> Result<bool> {
        let process = {
            let mut procs = self.processes.write().await;
            procs.remove(profile_id)
        };

        // Also stop the managed bridge
        self.stop_bridge(profile_id).await;

        match process {
            Some(proc) => {
                let _ = proc.stop_tx.send(true);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Restart a gateway (stop then start).
    pub async fn restart(&self, profile: &UserProfile) -> Result<()> {
        let _ = self.stop(&profile.id).await;
        // Small delay to let the process clean up
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        self.start(profile).await
    }

    /// Get the status of a gateway process.
    pub async fn status(&self, profile_id: &str) -> ProcessStatus {
        let procs = self.processes.read().await;
        match procs.get(profile_id) {
            Some(proc) => {
                let uptime = Utc::now() - proc.started_at;
                ProcessStatus {
                    running: true,
                    pid: Some(proc.pid),
                    started_at: Some(proc.started_at.to_rfc3339()),
                    uptime_secs: Some(uptime.num_seconds()),
                }
            }
            None => ProcessStatus {
                running: false,
                pid: None,
                started_at: None,
                uptime_secs: None,
            },
        }
    }

    /// Subscribe to log output for a profile. Returns None if not running.
    pub async fn subscribe_logs(&self, profile_id: &str) -> Option<broadcast::Receiver<String>> {
        let procs = self.processes.read().await;
        procs.get(profile_id).map(|p| p.log_tx.subscribe())
    }

    /// Get buffered log history for a profile. Returns empty vec if not running.
    pub async fn log_history(&self, profile_id: &str) -> Vec<String> {
        let procs = self.processes.read().await;
        match procs.get(profile_id) {
            Some(p) => p.log_history.lock().await.clone(),
            None => Vec::new(),
        }
    }

    /// Get the status of all profiles.
    pub async fn all_statuses(&self) -> HashMap<String, ProcessStatus> {
        let procs = self.processes.read().await;
        let mut statuses = HashMap::new();
        for (id, proc) in procs.iter() {
            let uptime = Utc::now() - proc.started_at;
            statuses.insert(
                id.clone(),
                ProcessStatus {
                    running: true,
                    pid: Some(proc.pid),
                    started_at: Some(proc.started_at.to_rfc3339()),
                    uptime_secs: Some(uptime.num_seconds()),
                },
            );
        }
        statuses
    }

    /// Stop all running gateways (and their bridges).
    ///
    /// Kills child processes directly by PID rather than relying on async
    /// monitor tasks, because the caller may call `std::process::exit()`
    /// immediately after this returns (which would abort tokio tasks before
    /// they can execute `child.kill()`).
    pub async fn stop_all(&self) -> usize {
        // Stop all bridges first
        let bridge_ids: Vec<String> = {
            let bridges = self.bridges.read().await;
            bridges.keys().cloned().collect()
        };
        for id in &bridge_ids {
            self.stop_bridge(id).await;
        }

        let processes: HashMap<String, GatewayProcess> = {
            let mut procs = self.processes.write().await;
            std::mem::take(&mut *procs)
        };
        let count = processes.len();
        for (id, proc) in processes {
            // Signal monitor task (best-effort, may not run before exit)
            let _ = proc.stop_tx.send(true);
            // Kill directly by PID so the child dies even if tokio exits
            // immediately after this method returns (e.g. std::process::exit).
            #[cfg(unix)]
            let _ = std::process::Command::new("kill")
                .arg(proc.pid.to_string())
                .status();
            #[cfg(windows)]
            let _ = std::process::Command::new("taskkill")
                .args(["/F", "/T", "/PID", &proc.pid.to_string()])
                .status();
            tracing::info!(profile = %id, pid = proc.pid, "gateway killed");
        }
        count
    }

    /// Get a reference to the underlying profile store.
    pub fn profile_store(&self) -> &Arc<ProfileStore> {
        &self.profile_store
    }

    /// Get the data directory path for a profile.
    pub fn resolve_data_dir(&self, profile: &UserProfile) -> PathBuf {
        self.profile_store.resolve_data_dir(profile)
    }

    /// Get the webhook port for a running gateway (if any).
    pub async fn webhook_port(&self, profile_id: &str) -> Option<u16> {
        let procs = self.processes.read().await;
        procs.get(profile_id).and_then(|p| p.webhook_port)
    }

    /// Get the API channel port for a profile.
    pub async fn api_port(&self, profile_id: &str) -> Option<u16> {
        let procs = self.processes.read().await;
        procs.get(profile_id).and_then(|p| p.api_port)
    }

    /// Find the first running profile that has an API channel port.
    pub async fn first_api_port(&self) -> Option<(String, u16)> {
        let procs = self.processes.read().await;
        for (id, proc) in procs.iter() {
            if let Some(port) = proc.api_port {
                return Some((id.clone(), port));
            }
        }
        None
    }

    /// Read provider QoS metrics for a profile from its data_dir/model_catalog.json.
    /// Returns `None` if the file doesn't exist or can't be parsed.
    pub async fn read_metrics(&self, profile_id: &str) -> Option<serde_json::Value> {
        let profile = self.profile_store.get(profile_id).ok()??;
        let data_dir = self.profile_store.resolve_data_dir(&profile);
        let path = data_dir.join("model_catalog.json");
        let content = tokio::fs::read_to_string(&path).await.ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Allocate the next available webhook port.
    fn allocate_webhook_port(&self, procs: &HashMap<String, GatewayProcess>) -> u16 {
        let used: std::collections::HashSet<u16> =
            procs.values().filter_map(|p| p.webhook_port).collect();
        let mut port = WEBHOOK_BASE_PORT;
        while used.contains(&port) || !port_available(port) {
            port += 1;
        }
        port
    }

    /// Allocate the next available API channel port (for serve mode auto-injection).
    fn allocate_api_port(&self, procs: &HashMap<String, GatewayProcess>) -> u16 {
        let used: std::collections::HashSet<u16> =
            procs.values().filter_map(|p| p.api_port).collect();
        let mut port = API_BASE_PORT;
        while used.contains(&port) || !port_available(port) {
            port += 1;
        }
        port
    }

    // ── Bridge lifecycle ───────────────────────────────────────────────

    /// Check if a profile has a WhatsApp channel that needs a managed bridge.
    /// A managed bridge is needed when bridge_url is empty or "auto".
    fn needs_wechat_bridge(&self, profile: &UserProfile) -> bool {
        profile
            .config
            .channels
            .iter()
            .any(|ch| matches!(ch, ChannelCredentials::WeChat { .. }))
    }

    fn needs_managed_bridge(&self, profile: &UserProfile) -> bool {
        if self.bridge_js_path.is_none() {
            return false;
        }
        profile.config.channels.iter().any(|ch| {
            matches!(ch,
                ChannelCredentials::WhatsApp { bridge_url }
                if bridge_url.is_empty() || bridge_url == "auto"
            )
        })
    }

    /// Internal: start a bridge process. Returns the WS port.
    async fn start_bridge_inner(&self, profile: &UserProfile) -> Result<u16> {
        let mut bridges = self.bridges.write().await;
        if let Some(existing) = bridges.get(&profile.id) {
            return Ok(existing.ws_port);
        }

        let bridge_js = self
            .bridge_js_path
            .as_ref()
            .ok_or_else(|| eyre::eyre!("bridge.js path not configured"))?;

        let (ws_port, http_port) = self.allocate_bridge_ports(&bridges);

        let data_dir = self.profile_store.resolve_data_dir(profile);
        let auth_dir = data_dir.join("whatsapp-auth");
        let media_dir = data_dir.join("whatsapp-media");
        std::fs::create_dir_all(&auth_dir)?;
        std::fs::create_dir_all(&media_dir)?;

        let node = find_node()?;
        let mut cmd = Command::new(&node);
        cmd.arg(bridge_js)
            .env("BRIDGE_PORT", ws_port.to_string())
            .env("MEDIA_PORT", http_port.to_string())
            .env("AUTH_DIR", &auth_dir)
            .env("MEDIA_DIR", &media_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn()?;
        let pid = child.id().unwrap_or(0);

        let (log_tx, _) = broadcast::channel::<String>(256);
        let (stop_tx, stop_rx) = watch::channel(false);
        let qr_code: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let status: Arc<Mutex<BridgeStatus>> = Arc::new(Mutex::new(BridgeStatus::Waiting));
        let phone_number: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let lid: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        let bridge = BridgeProcess {
            pid,
            ws_port,
            http_port,
            started_at: Utc::now(),
            qr_code: Arc::clone(&qr_code),
            status: Arc::clone(&status),
            phone_number: Arc::clone(&phone_number),
            lid: Arc::clone(&lid),
            log_tx: log_tx.clone(),
            stop_tx,
        };

        bridges.insert(profile.id.clone(), bridge);
        drop(bridges);

        // Spawn task to read stdout — parse JSON events for QR/status
        if let Some(stdout) = child.stdout.take() {
            let tx = log_tx.clone();
            let qr = Arc::clone(&qr_code);
            let st = Arc::clone(&status);
            let ph = Arc::clone(&phone_number);
            let li = Arc::clone(&lid);
            tokio::spawn(async move {
                let reader = BufReader::new(stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    // Try to parse structured JSON events from bridge
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&line) {
                        match json.get("type").and_then(|t| t.as_str()) {
                            Some("qr") => {
                                if let Some(qr_str) = json.get("qr").and_then(|q| q.as_str()) {
                                    *qr.lock().await = Some(qr_str.to_string());
                                    *st.lock().await = BridgeStatus::Waiting;
                                }
                            }
                            Some("status") => {
                                if let Some(s) = json.get("status").and_then(|s| s.as_str()) {
                                    match s {
                                        "connected" => {
                                            *qr.lock().await = None;
                                            *st.lock().await = BridgeStatus::Connected;
                                            if let Some(phone) =
                                                json.get("phone").and_then(|p| p.as_str())
                                            {
                                                *ph.lock().await = Some(phone.to_string());
                                            }
                                            if let Some(lid_val) =
                                                json.get("lid").and_then(|l| l.as_str())
                                            {
                                                *li.lock().await = Some(lid_val.to_string());
                                            }
                                        }
                                        "disconnected" => {
                                            *st.lock().await = BridgeStatus::Disconnected;
                                        }
                                        "logged_out" => {
                                            *qr.lock().await = None;
                                            *st.lock().await = BridgeStatus::LoggedOut;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    let _ = tx.send(line);
                }
            });
        }

        // Spawn task to read stderr
        if let Some(stderr) = child.stderr.take() {
            let tx = log_tx.clone();
            tokio::spawn(async move {
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let _ = tx.send(format!("[stderr] {line}"));
                }
            });
        }

        // Spawn task to wait for exit or stop
        let profile_id = profile.id.clone();
        let bridges_ref = Arc::clone(&self.bridges);
        tokio::spawn(async move {
            let mut stop_rx = stop_rx;
            tokio::select! {
                exit_status = child.wait() => {
                    bridges_ref.write().await.remove(&profile_id);
                    match exit_status {
                        Ok(s) => tracing::info!(profile = %profile_id, exit = %s, "bridge exited"),
                        Err(e) => tracing::error!(profile = %profile_id, error = %e, "bridge error"),
                    }
                }
                _ = stop_rx.changed() => {
                    let _ = child.kill().await;
                    tracing::info!(profile = %profile_id, "bridge stopped");
                }
            }
        });

        tracing::info!(
            profile = %profile.id,
            pid = pid,
            ws_port = ws_port,
            http_port = http_port,
            "managed WhatsApp bridge started"
        );

        // Small delay to let bridge start listening before gateway connects
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        Ok(ws_port)
    }

    /// Stop a managed bridge for a profile.
    /// Start a WeChat bridge subprocess. Returns the WS port.
    async fn start_wechat_bridge(&self, profile: &UserProfile) -> Result<u16> {
        // Hold the write lock for the entire operation to prevent concurrent
        // calls from allocating the same port (TOCTOU race).
        let mut bridges = self.bridges.write().await;
        let key = format!("{}-wechat", profile.id);
        if bridges.contains_key(&key) {
            // Already running — return existing port
            return Ok(bridges[&key].ws_port);
        }

        let ws_port = self.allocate_wechat_port(&bridges);

        // Resolve token from profile env_vars
        let token_env = profile
            .config
            .channels
            .iter()
            .find_map(|ch| {
                if let ChannelCredentials::WeChat { token_env, .. } = ch {
                    Some(token_env.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| "WECHAT_BOT_TOKEN".into());

        let token = profile
            .config
            .env_vars
            .get(&token_env)
            .cloned()
            .unwrap_or_default();

        // Find the wechat-bridge binary
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| std::path::PathBuf::from("."));

        let bridge_bin = if exe_dir.join("wechat-bridge").exists() {
            exe_dir.join("wechat-bridge")
        } else {
            // Check in bundled app-skills
            let bundled = self
                .profile_store
                .octos_home_dir()
                .join("bundled-app-skills/wechat-bridge/main");
            if bundled.exists() {
                bundled
            } else {
                // Try PATH
                std::path::PathBuf::from("wechat-bridge")
            }
        };

        tracing::info!(
            profile = %profile.id,
            port = ws_port,
            bridge = %bridge_bin.display(),
            "starting WeChat bridge"
        );

        let mut cmd = tokio::process::Command::new(&bridge_bin);
        cmd.arg("--port").arg(ws_port.to_string());
        if !token.is_empty() {
            cmd.arg("--token").arg(&token);
        }
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| eyre::eyre!("failed to spawn wechat-bridge: {e}"))?;

        let pid = child.id().unwrap_or(0);
        tracing::info!(profile = %profile.id, pid, port = ws_port, "WeChat bridge spawned");

        // Read stdout for status events
        let (log_tx, _) = tokio::sync::broadcast::channel::<String>(64);
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let qr_code = Arc::new(tokio::sync::Mutex::new(None::<String>));
        let status = Arc::new(tokio::sync::Mutex::new(BridgeStatus::Waiting));

        let qr_clone = qr_code.clone();
        let status_clone = status.clone();

        if let Some(stdout) = child.stdout.take() {
            let log_tx2 = log_tx.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let reader = tokio::io::BufReader::new(stdout);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let _ = log_tx2.send(line.clone());
                    if let Ok(evt) = serde_json::from_str::<serde_json::Value>(&line) {
                        match evt["type"].as_str() {
                            Some("qr") => {
                                if let Some(url) = evt["qr_url"].as_str() {
                                    *qr_clone.lock().await = Some(url.to_string());
                                }
                            }
                            Some("status") => match evt["status"].as_str() {
                                Some("connected" | "polling") => {
                                    *status_clone.lock().await = BridgeStatus::Connected;
                                }
                                Some("session_timeout") => {
                                    *status_clone.lock().await = BridgeStatus::Disconnected;
                                }
                                _ => {}
                            },
                            _ => {}
                        }
                    }
                }
            });
        }

        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(async move {
                use tokio::io::AsyncBufReadExt;
                let reader = tokio::io::BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "wechat-bridge", "{}", line);
                }
            });
        }

        // Insert into map before releasing write lock to prevent races.
        bridges.insert(
            key.clone(),
            BridgeProcess {
                pid,
                ws_port,
                http_port: ws_port + 1,
                started_at: chrono::Utc::now(),
                qr_code,
                status,
                phone_number: Arc::new(tokio::sync::Mutex::new(None)),
                lid: Arc::new(tokio::sync::Mutex::new(None)),
                log_tx,
                stop_tx,
            },
        );
        drop(bridges);

        // Monitor process lifecycle
        let key2 = key;
        let bridges_ref = self.bridges.clone();
        let mut stop_rx2 = stop_rx.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = child.wait() => {
                    tracing::warn!("WeChat bridge exited");
                }
                _ = stop_rx2.changed() => {
                    let _ = child.kill().await;
                }
            }
            bridges_ref.write().await.remove(&key2);
        });

        // Wait a moment for the bridge to start
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        Ok(ws_port)
    }

    async fn stop_bridge(&self, profile_id: &str) {
        let bridge = {
            let mut bridges = self.bridges.write().await;
            bridges.remove(profile_id)
        };
        if let Some(b) = bridge {
            let _ = b.stop_tx.send(true);
            tracing::info!(profile = %profile_id, "bridge stopped");
        }
    }

    /// Get the WhatsApp QR code and status for a profile.
    pub async fn bridge_qr(&self, profile_id: &str) -> Option<BridgeQrInfo> {
        let bridges = self.bridges.read().await;
        let bridge = bridges.get(profile_id)?;
        let qr = bridge.qr_code.lock().await.clone();
        let status = *bridge.status.lock().await;
        let phone_number = bridge.phone_number.lock().await.clone();
        let lid = bridge.lid.lock().await.clone();
        Some(BridgeQrInfo {
            qr,
            status,
            ws_port: bridge.ws_port,
            http_port: bridge.http_port,
            phone_number,
            lid,
        })
    }

    /// Allocate the next available port pair for a bridge.
    /// Checks both the in-memory bridge map and actual port availability.

    fn allocate_wechat_port(&self, bridges: &HashMap<String, BridgeProcess>) -> u16 {
        let used: std::collections::HashSet<u16> = bridges.values().map(|b| b.ws_port).collect();
        let mut port = 3201u16;
        while used.contains(&port) || !port_available(port) {
            port += 1;
        }
        port
    }
    fn allocate_bridge_ports(&self, bridges: &HashMap<String, BridgeProcess>) -> (u16, u16) {
        let used_ports: std::collections::HashSet<u16> =
            bridges.values().map(|b| b.ws_port).collect();
        let mut ws_port = BRIDGE_BASE_WS_PORT;
        loop {
            if !used_ports.contains(&ws_port)
                && port_available(ws_port)
                && port_available(ws_port + 1)
            {
                return (ws_port, ws_port + 1);
            }
            ws_port += 2;
        }
    }
}

/// Check if a TCP port is available by attempting to bind it.
fn port_available(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Find the `node` binary on PATH.
fn find_node() -> Result<PathBuf> {
    let candidates = ["node", "/opt/homebrew/bin/node", "/usr/local/bin/node"];
    for name in candidates {
        let p = Path::new(name);
        if p.is_absolute() && p.exists() {
            return Ok(p.to_path_buf());
        }
        // Try which/where
        #[cfg(windows)]
        let which_cmd = "where";
        #[cfg(not(windows))]
        let which_cmd = "which";
        if let Ok(output) = std::process::Command::new(which_cmd).arg(name).output() {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !path.is_empty() {
                    return Ok(PathBuf::from(path));
                }
            }
        }
    }
    bail!("node not found — install Node.js to use managed WhatsApp bridges")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::{ProfileConfig, ProfileStore, UserProfile};
    use chrono::Utc;
    use std::sync::Arc;

    /// Create a temporary ProfileStore backed by a real temp directory.
    fn temp_profile_store() -> (tempfile::TempDir, Arc<ProfileStore>) {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let store = ProfileStore::open(dir.path()).expect("failed to open profile store");
        (dir, Arc::new(store))
    }

    /// Build a minimal UserProfile for testing.
    fn test_profile(id: &str, channels: Vec<ChannelCredentials>) -> UserProfile {
        UserProfile {
            id: id.to_string(),
            name: id.to_string(),
            enabled: true,
            data_dir: None,
            parent_id: None,
            config: ProfileConfig {
                channels,
                ..Default::default()
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    fn make_pm() -> (tempfile::TempDir, ProcessManager) {
        let (dir, store) = temp_profile_store();
        (dir, ProcessManager::new(store))
    }

    // ── port_available ────────────────────────────────────────────────

    #[test]
    fn should_report_bound_port_as_unavailable() {
        let listener =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        assert!(!port_available(port));
    }

    #[test]
    fn should_report_free_port_as_available() {
        // Bind then drop to get a port that was recently free.
        let port = {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
                .expect("failed to bind ephemeral port");
            listener.local_addr().unwrap().port()
        };
        // Port should be free now (barring TIME_WAIT race, very unlikely on localhost).
        assert!(port_available(port));
    }

    // ── allocate_webhook_port ─────────────────────────────────────────

    #[test]
    fn should_allocate_webhook_port_from_base_when_no_existing() {
        let (_dir, pm) = make_pm();
        let procs = HashMap::new();
        let port = pm.allocate_webhook_port(&procs);
        assert_eq!(port, WEBHOOK_BASE_PORT);
    }

    #[test]
    fn should_skip_used_webhook_ports() {
        let (_dir, pm) = make_pm();
        let mut procs = HashMap::new();
        // Simulate a process using the base webhook port.
        let (log_tx, _) = broadcast::channel(1);
        let (stop_tx, _) = watch::channel(false);
        procs.insert(
            "existing".to_string(),
            GatewayProcess {
                pid: 1,
                started_at: Utc::now(),
                log_tx,
                stop_tx,
                webhook_port: Some(WEBHOOK_BASE_PORT),
                api_port: None,
                log_history: Arc::new(Mutex::new(Vec::new())),
            },
        );
        let port = pm.allocate_webhook_port(&procs);
        assert!(port > WEBHOOK_BASE_PORT);
        // Should not collide with the existing port.
        assert_ne!(port, WEBHOOK_BASE_PORT);
    }

    // ── allocate_api_port ─────────────────────────────────────────────

    #[test]
    fn should_allocate_api_port_from_base_when_no_existing() {
        let (_dir, pm) = make_pm();
        let procs = HashMap::new();
        let port = pm.allocate_api_port(&procs);
        assert_eq!(port, API_BASE_PORT);
    }

    #[test]
    fn should_skip_used_api_ports() {
        let (_dir, pm) = make_pm();
        let mut procs = HashMap::new();
        let (log_tx, _) = broadcast::channel(1);
        let (stop_tx, _) = watch::channel(false);
        procs.insert(
            "existing".to_string(),
            GatewayProcess {
                pid: 1,
                started_at: Utc::now(),
                log_tx,
                stop_tx,
                webhook_port: None,
                api_port: Some(API_BASE_PORT),
                log_history: Arc::new(Mutex::new(Vec::new())),
            },
        );
        let port = pm.allocate_api_port(&procs);
        assert!(port > API_BASE_PORT);
    }

    #[test]
    fn should_allocate_sequential_api_ports_for_multiple_profiles() {
        let (_dir, pm) = make_pm();
        let mut procs = HashMap::new();

        // Simulate two existing processes with sequential ports.
        for (i, id) in ["p1", "p2"].iter().enumerate() {
            let (log_tx, _) = broadcast::channel(1);
            let (stop_tx, _) = watch::channel(false);
            procs.insert(
                id.to_string(),
                GatewayProcess {
                    pid: i as u32 + 1,
                    started_at: Utc::now(),
                    log_tx,
                    stop_tx,
                    webhook_port: None,
                    api_port: Some(API_BASE_PORT + i as u16),
                    log_history: Arc::new(Mutex::new(Vec::new())),
                },
            );
        }
        let port = pm.allocate_api_port(&procs);
        // Should be at least API_BASE_PORT + 2 (skipping 0 and 1).
        assert!(port >= API_BASE_PORT + 2);
    }

    // ── allocate_bridge_ports ─────────────────────────────────────────

    #[test]
    fn should_allocate_bridge_ports_as_consecutive_pair() {
        let (_dir, pm) = make_pm();
        let bridges = HashMap::new();
        let (ws, http) = pm.allocate_bridge_ports(&bridges);
        assert_eq!(ws, BRIDGE_BASE_WS_PORT);
        assert_eq!(http, BRIDGE_BASE_WS_PORT + 1);
    }

    #[test]
    fn should_skip_used_bridge_ports() {
        let (_dir, pm) = make_pm();
        let mut bridges = HashMap::new();
        let (log_tx, _) = broadcast::channel(1);
        let (stop_tx, _) = watch::channel(false);
        bridges.insert(
            "existing".to_string(),
            BridgeProcess {
                pid: 1,
                ws_port: BRIDGE_BASE_WS_PORT,
                http_port: BRIDGE_BASE_WS_PORT + 1,
                started_at: Utc::now(),
                qr_code: Arc::new(Mutex::new(None)),
                status: Arc::new(Mutex::new(BridgeStatus::Waiting)),
                phone_number: Arc::new(Mutex::new(None)),
                lid: Arc::new(Mutex::new(None)),
                log_tx,
                stop_tx,
            },
        );
        let (ws, http) = pm.allocate_bridge_ports(&bridges);
        // Must skip past the pair used by "existing" (3101, 3102).
        assert!(ws > BRIDGE_BASE_WS_PORT);
        assert_eq!(http, ws + 1);
    }

    // ── allocate_wechat_port ──────────────────────────────────────────

    #[test]
    fn should_allocate_wechat_port_from_base() {
        let (_dir, pm) = make_pm();
        let bridges = HashMap::new();
        let port = pm.allocate_wechat_port(&bridges);
        assert_eq!(port, 3201);
    }

    // ── needs_managed_bridge ──────────────────────────────────────────

    #[test]
    fn should_need_bridge_when_whatsapp_url_empty_and_bridge_js_set() {
        let (dir, store) = temp_profile_store();
        let bridge_path = dir.path().join("bridge.js");
        std::fs::write(&bridge_path, "// stub").unwrap();
        let pm = ProcessManager::new(store).with_bridge_js(bridge_path);

        let profile = test_profile(
            "wa-auto",
            vec![ChannelCredentials::WhatsApp {
                bridge_url: String::new(),
            }],
        );
        assert!(pm.needs_managed_bridge(&profile));
    }

    #[test]
    fn should_need_bridge_when_whatsapp_url_is_auto() {
        let (dir, store) = temp_profile_store();
        let bridge_path = dir.path().join("bridge.js");
        std::fs::write(&bridge_path, "// stub").unwrap();
        let pm = ProcessManager::new(store).with_bridge_js(bridge_path);

        let profile = test_profile(
            "wa-auto2",
            vec![ChannelCredentials::WhatsApp {
                bridge_url: "auto".to_string(),
            }],
        );
        assert!(pm.needs_managed_bridge(&profile));
    }

    #[test]
    fn should_not_need_bridge_when_explicit_url() {
        let (dir, store) = temp_profile_store();
        let bridge_path = dir.path().join("bridge.js");
        std::fs::write(&bridge_path, "// stub").unwrap();
        let pm = ProcessManager::new(store).with_bridge_js(bridge_path);

        let profile = test_profile(
            "wa-explicit",
            vec![ChannelCredentials::WhatsApp {
                bridge_url: "ws://remote:3101".to_string(),
            }],
        );
        assert!(!pm.needs_managed_bridge(&profile));
    }

    #[test]
    fn should_not_need_bridge_when_no_bridge_js() {
        let (_dir, pm) = make_pm();
        let profile = test_profile(
            "wa-no-js",
            vec![ChannelCredentials::WhatsApp {
                bridge_url: String::new(),
            }],
        );
        assert!(!pm.needs_managed_bridge(&profile));
    }

    #[test]
    fn should_not_need_bridge_for_non_whatsapp_profile() {
        let (dir, store) = temp_profile_store();
        let bridge_path = dir.path().join("bridge.js");
        std::fs::write(&bridge_path, "// stub").unwrap();
        let pm = ProcessManager::new(store).with_bridge_js(bridge_path);

        let profile = test_profile(
            "tg",
            vec![ChannelCredentials::Telegram {
                token_env: "BOT_TOKEN".to_string(),
                allowed_senders: String::new(),
            }],
        );
        assert!(!pm.needs_managed_bridge(&profile));
    }

    // ── needs_wechat_bridge ───────────────────────────────────────────

    #[test]
    fn should_detect_wechat_channel() {
        let (_dir, pm) = make_pm();
        let profile = test_profile(
            "wc",
            vec![ChannelCredentials::WeChat {
                token_env: "TOKEN".to_string(),
                base_url: "http://localhost".to_string(),
            }],
        );
        assert!(pm.needs_wechat_bridge(&profile));
    }

    #[test]
    fn should_not_detect_wechat_for_telegram() {
        let (_dir, pm) = make_pm();
        let profile = test_profile(
            "tg",
            vec![ChannelCredentials::Telegram {
                token_env: "BOT_TOKEN".to_string(),
                allowed_senders: String::new(),
            }],
        );
        assert!(!pm.needs_wechat_bridge(&profile));
    }

    // ── with_serve_config ─────────────────────────────────────────────

    #[test]
    fn should_set_serve_port_and_admin_token() {
        let (_dir, store) = temp_profile_store();
        let pm = ProcessManager::new(store).with_serve_config(8080, Some("secret".to_string()));
        assert_eq!(pm.serve_port, Some(8080));
        assert_eq!(pm.admin_token.as_deref(), Some("secret"));
    }

    #[test]
    fn should_set_serve_config_without_token() {
        let (_dir, store) = temp_profile_store();
        let pm = ProcessManager::new(store).with_serve_config(9090, None);
        assert_eq!(pm.serve_port, Some(9090));
        assert!(pm.admin_token.is_none());
    }

    // ── with_bridge_js ────────────────────────────────────────────────

    #[test]
    fn should_set_bridge_js_path_when_file_exists() {
        let (dir, store) = temp_profile_store();
        let bridge_path = dir.path().join("bridge.js");
        std::fs::write(&bridge_path, "// stub").unwrap();
        let pm = ProcessManager::new(store).with_bridge_js(bridge_path.clone());
        assert_eq!(pm.bridge_js_path, Some(bridge_path));
    }

    #[test]
    fn should_ignore_bridge_js_when_file_missing() {
        let (_dir, store) = temp_profile_store();
        let pm = ProcessManager::new(store).with_bridge_js(PathBuf::from("/nonexistent/bridge.js"));
        assert!(pm.bridge_js_path.is_none());
    }

    // ── ProcessStatus / BridgeStatus ──────────────────────────────────

    #[test]
    fn should_serialize_process_status_not_running() {
        let status = ProcessStatus {
            running: false,
            pid: None,
            started_at: None,
            uptime_secs: None,
        };
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["running"], false);
        assert!(json["pid"].is_null());
    }

    #[test]
    fn should_serialize_bridge_status_variants() {
        assert_eq!(
            serde_json::to_value(BridgeStatus::Waiting)
                .unwrap()
                .as_str()
                .unwrap(),
            "waiting"
        );
        assert_eq!(
            serde_json::to_value(BridgeStatus::Connected)
                .unwrap()
                .as_str()
                .unwrap(),
            "connected"
        );
        assert_eq!(
            serde_json::to_value(BridgeStatus::Disconnected)
                .unwrap()
                .as_str()
                .unwrap(),
            "disconnected"
        );
        assert_eq!(
            serde_json::to_value(BridgeStatus::LoggedOut)
                .unwrap()
                .as_str()
                .unwrap(),
            "logged_out"
        );
    }

    // ── Async tests (status, stop, log_history on empty PM) ──────────

    #[tokio::test]
    async fn should_return_not_running_for_unknown_profile() {
        let (_dir, pm) = make_pm();
        let status = pm.status("nonexistent").await;
        assert!(!status.running);
        assert!(status.pid.is_none());
    }

    #[tokio::test]
    async fn should_return_false_when_stopping_nonexistent_profile() {
        let (_dir, pm) = make_pm();
        let stopped = pm.stop("nonexistent").await.unwrap();
        assert!(!stopped);
    }

    #[tokio::test]
    async fn should_return_empty_log_history_for_unknown_profile() {
        let (_dir, pm) = make_pm();
        let history = pm.log_history("nonexistent").await;
        assert!(history.is_empty());
    }

    #[tokio::test]
    async fn should_return_none_for_unknown_webhook_port() {
        let (_dir, pm) = make_pm();
        assert!(pm.webhook_port("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn should_return_none_for_unknown_api_port() {
        let (_dir, pm) = make_pm();
        assert!(pm.api_port("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn should_return_none_for_first_api_port_when_empty() {
        let (_dir, pm) = make_pm();
        assert!(pm.first_api_port().await.is_none());
    }

    #[tokio::test]
    async fn should_return_empty_statuses_when_no_processes() {
        let (_dir, pm) = make_pm();
        let statuses = pm.all_statuses().await;
        assert!(statuses.is_empty());
    }

    #[tokio::test]
    async fn should_stop_all_return_zero_when_empty() {
        let (_dir, pm) = make_pm();
        let count = pm.stop_all().await;
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn should_return_none_for_unknown_bridge_qr() {
        let (_dir, pm) = make_pm();
        assert!(pm.bridge_qr("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn should_return_none_for_subscribe_logs_unknown() {
        let (_dir, pm) = make_pm();
        assert!(pm.subscribe_logs("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn should_return_none_for_read_metrics_unknown() {
        let (_dir, pm) = make_pm();
        assert!(pm.read_metrics("nonexistent").await.is_none());
    }

    // ── No port collision across allocation types ─────────────────────

    #[test]
    fn should_have_distinct_base_ports() {
        // Verify that the base port constants are all different.
        let ports = [BRIDGE_BASE_WS_PORT, WEBHOOK_BASE_PORT, API_BASE_PORT];
        for i in 0..ports.len() {
            for j in (i + 1)..ports.len() {
                assert_ne!(
                    ports[i], ports[j],
                    "base port collision at index {i} and {j}"
                );
            }
        }
        // Bridge ports are in the 3000s, webhook/api in the 9000s.
        assert!(BRIDGE_BASE_WS_PORT < WEBHOOK_BASE_PORT);
        assert!(WEBHOOK_BASE_PORT < API_BASE_PORT);
    }
}
