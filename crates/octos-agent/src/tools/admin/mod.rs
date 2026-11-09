//! Admin API tools for admin-mode gateways.
//!
//! Each sub-module implements one or more related admin tools.
//! All tools call the `octos serve` REST API via [`AdminApiContext`].

mod platform_skills;
mod profiles;
mod skills;
mod sub_accounts;
mod system;
mod update;

use std::sync::Arc;

use eyre::Result;
use serde::Deserialize;

use super::{Tool, ToolRegistry, ToolResult};

/// Shared context for all admin API tools.
pub struct AdminApiContext {
    pub http: reqwest::Client,
    pub serve_url: String,
    pub admin_token: String,
}

impl AdminApiContext {
    /// Make an authenticated GET request.
    pub(crate) async fn get(&self, path: &str) -> Result<serde_json::Value> {
        let url = format!("{}{}", self.serve_url, path);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(&self.admin_token)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            eyre::bail!("API error {}: {}", status, body);
        }
        Ok(resp.json().await?)
    }

    /// Make an authenticated POST request.
    pub(crate) async fn post(
        &self,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let url = format!("{}{}", self.serve_url, path);
        let mut req = self.http.post(&url).bearer_auth(&self.admin_token);
        if let Some(b) = body {
            req = req.json(b);
        } else {
            req = req.header("content-type", "application/json");
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            eyre::bail!("API error {}: {}", status, body);
        }
        Ok(resp.json().await?)
    }

    /// Make an authenticated DELETE request.
    pub(crate) async fn delete(&self, path: &str) -> Result<serde_json::Value> {
        let url = format!("{}{}", self.serve_url, path);
        let resp = self
            .http
            .delete(&url)
            .bearer_auth(&self.admin_token)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            eyre::bail!("API error {}: {}", status, body);
        }
        Ok(resp.json().await?)
    }

    /// Make an authenticated PUT request.
    pub(crate) async fn put(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let url = format!("{}{}", self.serve_url, path);
        let resp = self
            .http
            .put(&url)
            .bearer_auth(&self.admin_token)
            .json(body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            eyre::bail!("API error {}: {}", status, body);
        }
        Ok(resp.json().await?)
    }
}

// ── Shared helpers ──────────────────────────────────────────────────

pub(crate) fn format_duration(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else if secs < 86400 {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d {}h", secs / 86400, (secs % 86400) / 3600)
    }
}

// ── Shared input types ──────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct ProfileIdInput {
    pub profile_id: String,
}

// ── Registration ────────────────────────────────────────────────────

/// Register all admin API tools into a ToolRegistry.
pub fn register_admin_api_tools(registry: &mut ToolRegistry, ctx: Arc<AdminApiContext>) {
    // Profile management
    registry.register(profiles::ListProfilesTool::new(ctx.clone()));
    registry.register(profiles::ProfileStatusTool::new(ctx.clone()));
    registry.register(profiles::StartProfileTool::new(ctx.clone()));
    registry.register(profiles::StopProfileTool::new(ctx.clone()));
    registry.register(profiles::RestartProfileTool::new(ctx.clone()));
    registry.register(profiles::EnableProfileTool::new(ctx.clone()));
    registry.register(profiles::UpdateProfileTool::new(ctx.clone()));

    // System monitoring
    registry.register(system::ViewLogsTool::new(ctx.clone()));
    registry.register(system::SystemHealthTool::new(ctx.clone()));
    registry.register(system::SystemMetricsTool::new(ctx.clone()));
    registry.register(system::ProviderMetricsTool::new(ctx.clone()));
    registry.register(system::ManageWatchdogTool::new(ctx.clone()));

    // Diagnostics
    registry.register(system::ViewSessionsTool::new(ctx.clone()));
    registry.register(system::CronStatusTool::new(ctx.clone()));
    registry.register(system::CheckConfigTool::new(ctx.clone()));

    // Sub-accounts
    registry.register(sub_accounts::ListSubAccountsTool::new(ctx.clone()));
    registry.register(sub_accounts::CreateSubAccountTool::new(ctx.clone()));

    // Skills
    registry.register(skills::ManageSkillsTool::new(ctx.clone()));

    // Platform skills (ASR/TTS engine management)
    registry.register(platform_skills::PlatformSkillsTool::new(ctx.clone()));

    // System update
    registry.register(update::UpdateOctosTool::new(ctx));
}
