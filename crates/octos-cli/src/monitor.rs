//! Watchdog auto-restart, periodic health checks, and alert delivery.
//!
//! Extracted from admin_bot to run independently in `octos serve`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::{Mutex, mpsc};

use crate::process_manager::ProcessManager;
use crate::profiles::ProfileStore;

/// Alert types sent by ProcessManager or health checker.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AdminAlert {
    GatewayExited {
        profile_id: String,
        exit_code: Option<i32>,
        timestamp: DateTime<Utc>,
    },
    ProfileDown {
        profile_id: String,
        profile_name: String,
    },
    WatchdogRestarted {
        profile_id: String,
        attempt: u32,
    },
    WatchdogGaveUp {
        profile_id: String,
        attempts: u32,
    },
}

impl AdminAlert {
    /// Human-readable alert message.
    pub fn message(&self) -> String {
        match self {
            Self::GatewayExited {
                profile_id,
                exit_code,
                ..
            } => {
                let code = exit_code
                    .map(|c| format!(" (exit code {c})"))
                    .unwrap_or_default();
                format!("Gateway '{profile_id}' exited{code}")
            }
            Self::ProfileDown { profile_name, .. } => {
                format!("Profile '{profile_name}' is down (enabled but not running)")
            }
            Self::WatchdogRestarted {
                profile_id,
                attempt,
            } => {
                format!("Watchdog restarting '{profile_id}' (attempt {attempt})")
            }
            Self::WatchdogGaveUp {
                profile_id,
                attempts,
            } => {
                format!("Watchdog gave up on '{profile_id}' after {attempts} attempts")
            }
        }
    }
}

/// Trait for sending alerts to messaging channels.
#[async_trait::async_trait]
pub trait AlertSender: Send + Sync {
    async fn send_alert(&self, message: &str);
}

/// Per-profile restart tracking: (attempt_count, last_restart_time).
type RestartState = HashMap<String, (u32, DateTime<Utc>)>;

/// Monitor runs three concurrent tasks: alert receiver, health checker, and
/// restart-count resetter.
#[allow(clippy::too_many_arguments)]
pub struct Monitor {
    profile_store: Arc<ProfileStore>,
    process_manager: Arc<ProcessManager>,
    alert_rx: mpsc::Receiver<AdminAlert>,
    alert_senders: Vec<Box<dyn AlertSender>>,
    watchdog_enabled: Arc<AtomicBool>,
    alerts_enabled: Arc<AtomicBool>,
    max_restart_attempts: u32,
    restart_counts: Arc<Mutex<RestartState>>,
    health_interval: Duration,
    shutdown: Arc<AtomicBool>,
}

impl Monitor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        profile_store: Arc<ProfileStore>,
        process_manager: Arc<ProcessManager>,
        alert_rx: mpsc::Receiver<AdminAlert>,
        watchdog_enabled: Arc<AtomicBool>,
        alerts_enabled: Arc<AtomicBool>,
        max_restart_attempts: u32,
        health_interval: Duration,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            profile_store,
            process_manager,
            alert_rx,
            alert_senders: Vec::new(),
            watchdog_enabled,
            alerts_enabled,
            max_restart_attempts,
            restart_counts: Arc::new(Mutex::new(HashMap::new())),
            health_interval,
            shutdown,
        }
    }

    pub fn add_sender(&mut self, sender: Box<dyn AlertSender>) {
        self.alert_senders.push(sender);
    }

    /// Run the monitor. Consumes self. Returns when shutdown is signaled.
    pub async fn run(mut self) {
        let senders = Arc::new(self.alert_senders);
        let restart_counts = self.restart_counts.clone();
        let pm = self.process_manager.clone();
        let ps = self.profile_store.clone();
        let wd = self.watchdog_enabled.clone();
        let al = self.alerts_enabled.clone();
        let max_attempts = self.max_restart_attempts;
        let shutdown = self.shutdown.clone();

        // Task 1: Alert receiver — handles gateway exits
        let senders1 = senders.clone();
        let rc1 = restart_counts.clone();
        let pm1 = pm.clone();
        let ps1 = ps.clone();
        let wd1 = wd.clone();
        let al1 = al.clone();
        let shutdown1 = shutdown.clone();
        let alert_task = tokio::spawn(async move {
            while let Some(alert) = self.alert_rx.recv().await {
                if shutdown1.load(Ordering::Acquire) {
                    break;
                }

                // Send alert to all channels
                if al1.load(Ordering::Relaxed) {
                    let msg = alert.message();
                    for sender in senders1.iter() {
                        sender.send_alert(&msg).await;
                    }
                }

                // Watchdog: auto-restart on GatewayExited
                if let AdminAlert::GatewayExited { ref profile_id, .. } = alert {
                    if wd1.load(Ordering::Relaxed) {
                        let mut counts = rc1.lock().await;
                        let (count, _) =
                            counts.entry(profile_id.clone()).or_insert((0, Utc::now()));
                        *count += 1;
                        let attempt = *count;

                        if attempt <= max_attempts {
                            tracing::info!(
                                profile = %profile_id,
                                attempt = attempt,
                                "watchdog auto-restarting gateway"
                            );

                            // Send restart notification
                            if al1.load(Ordering::Relaxed) {
                                let restart_msg = AdminAlert::WatchdogRestarted {
                                    profile_id: profile_id.clone(),
                                    attempt,
                                }
                                .message();
                                for sender in senders1.iter() {
                                    sender.send_alert(&restart_msg).await;
                                }
                            }

                            // Backoff: 2^attempt seconds, capped at 30s
                            let backoff = Duration::from_secs((2u64.pow(attempt)).min(30));
                            tokio::time::sleep(backoff).await;

                            if let Ok(Some(profile)) = ps1.get(profile_id) {
                                if let Err(e) = pm1.start(&profile).await {
                                    tracing::warn!(
                                        profile = %profile_id,
                                        error = %e,
                                        "watchdog restart failed"
                                    );
                                }
                            }

                            // Update last-restart timestamp
                            if let Some(entry) = counts.get_mut(profile_id) {
                                entry.1 = Utc::now();
                            }
                        } else {
                            tracing::warn!(
                                profile = %profile_id,
                                attempts = attempt,
                                "watchdog giving up"
                            );
                            if al1.load(Ordering::Relaxed) {
                                let giveup_msg = AdminAlert::WatchdogGaveUp {
                                    profile_id: profile_id.clone(),
                                    attempts: attempt,
                                }
                                .message();
                                for sender in senders1.iter() {
                                    sender.send_alert(&giveup_msg).await;
                                }
                            }
                        }
                    }
                }
            }
        });

        // Task 2: Periodic health check
        let senders2 = senders.clone();
        let rc2 = restart_counts.clone();
        let shutdown2 = shutdown.clone();
        let health_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(self.health_interval);
            // Skip the initial immediate tick
            interval.tick().await;

            loop {
                interval.tick().await;
                if shutdown2.load(Ordering::Acquire) {
                    break;
                }

                let profiles = ps.list().unwrap_or_default();
                let statuses = pm.all_statuses().await;

                for p in &profiles {
                    if p.enabled && !statuses.contains_key(&p.id) {
                        // Check if watchdog already knows about this
                        let counts = rc2.lock().await;
                        let already_tracked = counts.get(&p.id).is_some_and(|(c, _)| *c > 0);
                        drop(counts);

                        if !already_tracked && al.load(Ordering::Relaxed) {
                            let msg = AdminAlert::ProfileDown {
                                profile_id: p.id.clone(),
                                profile_name: p.name.clone(),
                            }
                            .message();
                            for sender in senders2.iter() {
                                sender.send_alert(&msg).await;
                            }
                        }
                    }
                }

                // Reset restart counts for profiles stable > 5 minutes
                let now = Utc::now();
                let mut counts = rc2.lock().await;
                counts.retain(|id, (_, last_restart)| {
                    let stable_for = now - *last_restart;
                    if stable_for.num_seconds() > 300 && statuses.get(id).is_some_and(|s| s.running)
                    {
                        tracing::debug!(profile = %id, "restart count reset (stable > 5 min)");
                        false
                    } else {
                        true
                    }
                });
            }
        });

        // Wait for either task to complete (shouldn't happen unless shutdown)
        tokio::select! {
            _ = alert_task => {}
            _ = health_task => {}
        }
    }
}

// ── Alert sender implementations ───────────────────────────────────────

/// Sends alerts to a Telegram chat via HTTP API (no teloxide dependency).
pub struct TelegramAlertSender {
    token: String,
    chat_ids: Vec<i64>,
    http: reqwest::Client,
}

impl TelegramAlertSender {
    pub fn new(token: String, chat_ids: Vec<i64>) -> Self {
        Self {
            token,
            chat_ids,
            http: reqwest::Client::new(),
        }
    }
}

#[async_trait::async_trait]
impl AlertSender for TelegramAlertSender {
    async fn send_alert(&self, message: &str) {
        for chat_id in &self.chat_ids {
            let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
            let body = serde_json::json!({
                "chat_id": chat_id,
                "text": message,
            });
            if let Err(e) = self.http.post(&url).json(&body).send().await {
                tracing::warn!(chat_id = %chat_id, error = %e, "failed to send Telegram alert");
            }
        }
    }
}

/// Sends alerts to a Feishu chat via REST API.
pub struct FeishuAlertSender {
    app_id: String,
    app_secret: String,
    user_ids: Vec<String>,
    http: reqwest::Client,
    region: String,
}

impl FeishuAlertSender {
    pub fn new(app_id: String, app_secret: String, user_ids: Vec<String>, region: &str) -> Self {
        Self {
            app_id,
            app_secret,
            user_ids,
            http: reqwest::Client::new(),
            region: region.to_string(),
        }
    }

    fn base_url(&self) -> &str {
        if self.region == "global" {
            "https://open.larksuite.com"
        } else {
            "https://open.feishu.cn"
        }
    }

    async fn get_tenant_token(&self) -> Option<String> {
        let url = format!(
            "{}/open-apis/auth/v3/tenant_access_token/internal",
            self.base_url()
        );
        let resp = self
            .http
            .post(&url)
            .json(&serde_json::json!({
                "app_id": self.app_id,
                "app_secret": self.app_secret,
            }))
            .send()
            .await
            .ok()?;
        let body: serde_json::Value = resp.json().await.ok()?;
        body.get("tenant_access_token")
            .and_then(|t| t.as_str())
            .map(String::from)
    }
}

#[async_trait::async_trait]
impl AlertSender for FeishuAlertSender {
    async fn send_alert(&self, message: &str) {
        let token = match self.get_tenant_token().await {
            Some(t) => t,
            None => {
                tracing::warn!("failed to get Feishu tenant token for alert");
                return;
            }
        };

        let url = format!("{}/open-apis/im/v1/messages", self.base_url());
        for user_id in &self.user_ids {
            let body = serde_json::json!({
                "receive_id": user_id,
                "msg_type": "text",
                "content": serde_json::json!({"text": message}).to_string(),
            });
            if let Err(e) = self
                .http
                .post(&url)
                .query(&[("receive_id_type", "open_id")])
                .bearer_auth(&token)
                .json(&body)
                .send()
                .await
            {
                tracing::warn!(user_id = %user_id, error = %e, "failed to send Feishu alert");
            }
        }
    }
}
