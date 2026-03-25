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
/// Create a dummy `AdminApiContext` for unit tests that only check metadata.
#[cfg(test)]
pub(crate) fn test_ctx() -> Arc<AdminApiContext> {
    Arc::new(AdminApiContext {
        http: reqwest::Client::new(),
        serve_url: "http://localhost:0".into(),
        admin_token: "test-token".into(),
    })
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn read_http_request(stream: &mut tokio::net::TcpStream) -> String {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];
        let mut header_end = None;
        let mut content_length = 0_usize;

        loop {
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(n > 0, "connection closed before request was fully received");
            buffer.extend_from_slice(&chunk[..n]);

            if header_end.is_none() {
                if let Some(pos) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                    let end = pos + 4;
                    header_end = Some(end);
                    let headers = String::from_utf8_lossy(&buffer[..end]).to_lowercase();
                    content_length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                }
            }

            if let Some(end) = header_end {
                if buffer.len() >= end + content_length {
                    break;
                }
            }
        }

        String::from_utf8(buffer).unwrap()
    }

    async fn spawn_json_server(
        response_body: serde_json::Value,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = serde_json::to_vec(&response_body).unwrap();

        let handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
            request
        });

        (format!("http://{}", addr), handle)
    }

    #[test]
    fn format_duration_seconds() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(42), "42s");
        assert_eq!(format_duration(59), "59s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration(60), "1m 0s");
        assert_eq!(format_duration(90), "1m 30s");
        assert_eq!(format_duration(3599), "59m 59s");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(format_duration(3600), "1h 0m");
        assert_eq!(format_duration(7200), "2h 0m");
        assert_eq!(format_duration(3661), "1h 1m");
        assert_eq!(format_duration(86399), "23h 59m");
    }

    #[test]
    fn format_duration_days() {
        assert_eq!(format_duration(86400), "1d 0h");
        assert_eq!(format_duration(90000), "1d 1h");
        assert_eq!(format_duration(172800), "2d 0h");
    }

    #[test]
    fn profile_id_input_deserialize() {
        let v = serde_json::json!({"profile_id": "abc-123"});
        let input: ProfileIdInput = serde_json::from_value(v).unwrap();
        assert_eq!(input.profile_id, "abc-123");
    }

    #[test]
    fn profile_id_input_missing_field() {
        let v = serde_json::json!({});
        assert!(serde_json::from_value::<ProfileIdInput>(v).is_err());
    }

    #[test]
    fn register_admin_tools_populates_registry() {
        let ctx = test_ctx();
        let mut registry = ToolRegistry::new();
        register_admin_api_tools(&mut registry, ctx);

        let expected_names = [
            "admin_list_profiles",
            "admin_profile_status",
            "admin_start_profile",
            "admin_stop_profile",
            "admin_restart_profile",
            "admin_enable_profile",
            "admin_update_profile",
            "admin_view_logs",
            "admin_system_health",
            "admin_system_metrics",
            "admin_provider_metrics",
            "admin_manage_watchdog",
            "admin_view_sessions",
            "admin_cron_status",
            "admin_check_config",
            "admin_list_sub_accounts",
            "admin_create_sub_account",
            "admin_manage_skills",
            "admin_platform_skills",
            "admin_update_octos",
        ];

        let specs = registry.specs();
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        for expected in &expected_names {
            assert!(names.contains(expected), "missing tool: {expected}");
        }
        assert_eq!(specs.len(), expected_names.len());
    }
}
