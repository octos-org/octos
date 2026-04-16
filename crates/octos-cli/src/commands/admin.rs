//! Admin commands for tenant, tunnel, and operator management.

use std::time::Duration;

use clap::{Args, Subcommand};
use colored::Colorize;
use eyre::{Result, bail};
use uuid::Uuid;

use super::Executable;
use crate::tenant::{TenantConfig, TenantStatus, TenantStore, render_frpc_config};

/// Admin commands for tenant and tunnel management.
#[derive(Debug, Args)]
pub struct AdminCommand {
    #[command(subcommand)]
    pub action: AdminAction,
}

#[derive(Debug, Subcommand)]
pub enum AdminAction {
    /// Create a new tunnel tenant (assigns subdomain, auth token, and SSH port).
    CreateTenant {
        /// Tenant name (used as subdomain and ID).
        #[arg(long)]
        name: String,
        /// Base domain for the tunnel (default: octos-cloud.org).
        #[arg(long, default_value = "octos-cloud.org")]
        domain: String,
        /// frps server address (VPS IP or hostname).
        #[arg(long, default_value = "163.192.33.32")]
        server: String,
        /// frps control port.
        #[arg(long, default_value = "7000")]
        port: u16,
        /// Local octos serve port on the tenant machine. Matches the
        /// `octos serve` default (see serve.rs / issue #417).
        #[arg(long, default_value = "50080")]
        local_port: u16,
        /// Dashboard auth token (auto-generated if not provided).
        #[arg(long)]
        auth_token: Option<String>,
        /// Data directory override.
        #[arg(long)]
        data_dir: Option<std::path::PathBuf>,
    },
    /// List all registered tunnel tenants.
    ListTenants {
        /// Data directory override.
        #[arg(long)]
        data_dir: Option<std::path::PathBuf>,
    },
    /// Delete a tunnel tenant.
    DeleteTenant {
        /// Tenant ID to delete.
        name: String,
        /// Data directory override.
        #[arg(long)]
        data_dir: Option<std::path::PathBuf>,
    },
    /// Show the frpc config for a tenant.
    ShowTenantConfig {
        /// Tenant ID.
        name: String,
        /// Base domain for the tunnel.
        #[arg(long, default_value = "octos-cloud.org")]
        domain: String,
        /// frps server address.
        #[arg(long, default_value = "163.192.33.32")]
        server: String,
        /// frps control port.
        #[arg(long, default_value = "7000")]
        port: u16,
        /// Shared FRPS auth token for the central host.
        #[arg(long)]
        frps_token: Option<String>,
        /// Data directory override.
        #[arg(long)]
        data_dir: Option<std::path::PathBuf>,
    },
    /// Show a condensed operator view of runtime observability counters.
    OperatorSummary {
        /// Base URL of the running octos API.
        #[arg(long)]
        base_url: Option<String>,
        /// Admin or user bearer token for the API.
        #[arg(long)]
        auth_token: Option<String>,
        /// Emit raw JSON instead of a human summary.
        #[arg(long)]
        json: bool,
    },
}

impl Executable for AdminCommand {
    fn execute(self) -> Result<()> {
        match self.action {
            AdminAction::CreateTenant {
                name,
                domain,
                server: _,
                port: _,
                local_port,
                auth_token: auth_token_arg,
                data_dir,
            } => {
                let data_dir = super::resolve_data_dir(data_dir)?;
                let store = TenantStore::open(&data_dir)?;

                // Check for duplicate
                if store.get(&name)?.is_some() {
                    bail!("tenant '{name}' already exists");
                }

                let ssh_port = store.next_ssh_port()?;
                let auth_token = auth_token_arg.unwrap_or_else(|| {
                    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
                });
                let now = chrono::Utc::now();

                let tenant = TenantConfig {
                    id: name.clone(),
                    name: name.clone(),
                    subdomain: name.clone(),
                    tunnel_token: String::new(),
                    ssh_port,
                    local_port,
                    auth_token: auth_token.clone(),
                    owner: String::new(),
                    status: TenantStatus::Pending,
                    created_at: now,
                    updated_at: now,
                };

                store.save(&tenant)?;

                println!("Tenant created:");
                println!("  ID:         {}", tenant.id);
                println!("  Subdomain:  {}.{}", tenant.subdomain, domain);
                println!("  SSH port:   {}", tenant.ssh_port);
                println!("  Auth token: {}", auth_token);
                println!();
                println!("Bootstrap the Mac Mini:");
                println!(
                    "  ./scripts/frp/bootstrap-tenant.sh {} <user@host> --password <pw>",
                    tenant.id
                );
                println!();
                println!("Dashboard will be at:");
                println!("  http://{}.{}", tenant.subdomain, domain);
                println!("  Auth token: {}", auth_token);

                Ok(())
            }
            AdminAction::ListTenants { data_dir } => {
                let data_dir = super::resolve_data_dir(data_dir)?;
                let store = TenantStore::open(&data_dir)?;
                let tenants = store.list()?;

                if tenants.is_empty() {
                    println!("No tenants registered.");
                    return Ok(());
                }

                println!(
                    "{:<16} {:<24} {:<8} {:<10} {:<10}",
                    "ID", "SUBDOMAIN", "SSH", "STATUS", "CREATED"
                );
                for t in &tenants {
                    println!(
                        "{:<16} {:<24} {:<8} {:<10} {:<10}",
                        t.id,
                        t.subdomain,
                        t.ssh_port,
                        t.status,
                        t.created_at.format("%Y-%m-%d"),
                    );
                }
                println!("\n{} tenant(s)", tenants.len());

                Ok(())
            }
            AdminAction::DeleteTenant { name, data_dir } => {
                let data_dir = super::resolve_data_dir(data_dir)?;
                let store = TenantStore::open(&data_dir)?;

                if store.delete(&name)? {
                    println!("Tenant '{name}' deleted.");
                } else {
                    println!("Tenant '{name}' not found.");
                }

                Ok(())
            }
            AdminAction::ShowTenantConfig {
                name,
                domain,
                server,
                port,
                frps_token,
                data_dir,
            } => {
                let data_dir = super::resolve_data_dir(data_dir)?;
                let store = TenantStore::open(&data_dir)?;

                let tenant = store
                    .get(&name)?
                    .ok_or_else(|| eyre::eyre!("tenant '{name}' not found"))?;

                let frps_token = frps_token
                    .or_else(|| std::env::var("FRPS_TOKEN").ok())
                    .ok_or_else(|| {
                        eyre::eyre!(
                            "shared FRPS token is required; pass --frps-token or set FRPS_TOKEN"
                        )
                    })?;

                let config = render_frpc_config(&tenant, &server, port, &domain, &frps_token);
                println!("{config}");

                Ok(())
            }
            AdminAction::OperatorSummary {
                base_url,
                auth_token,
                json,
            } => {
                let base_url = resolve_base_url(base_url);
                let auth_token = resolve_auth_token(auth_token);
                let summary = fetch_operator_summary(&base_url, auth_token.as_deref())?;

                if json {
                    println!("{}", serde_json::to_string_pretty(&summary)?);
                } else {
                    print_operator_summary(&base_url, &summary);
                }

                Ok(())
            }
        }
    }
}

fn resolve_base_url(cli_value: Option<String>) -> String {
    cli_value
        .or_else(|| std::env::var("OCTOS_BASE_URL").ok())
        .or_else(|| std::env::var("OCTOS_TEST_URL").ok())
        .unwrap_or_else(|| "http://127.0.0.1:3000".to_string())
}

fn resolve_auth_token(cli_value: Option<String>) -> Option<String> {
    cli_value
        .or_else(|| std::env::var("OCTOS_AUTH_TOKEN").ok())
        .filter(|value| !value.trim().is_empty())
}

fn fetch_operator_summary(base_url: &str, auth_token: Option<&str>) -> Result<serde_json::Value> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let url = format!(
        "{}/api/admin/operator/summary",
        base_url.trim_end_matches('/')
    );
    let mut request = client.get(url);
    if let Some(token) = auth_token {
        request = request.bearer_auth(token);
    }

    let response = request.send()?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        bail!(
            "operator summary request failed: {status} {}",
            body.lines().next().unwrap_or_default()
        );
    }

    Ok(response.json()?)
}

fn print_operator_summary(base_url: &str, summary: &serde_json::Value) {
    println!("{}", "octos Operator Summary".cyan().bold());
    println!("{}", "═".repeat(60));
    println!("{}: {}", "Base URL".green(), base_url);
    println!(
        "{}: {}",
        "Metrics".green(),
        if summary
            .get("available")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            "available".green().to_string()
        } else {
            "no samples yet".yellow().to_string()
        }
    );
    println!();

    println!("{}", "Totals".cyan().bold());
    println!("{}", "─".repeat(60).dimmed());
    if let Some(totals) = summary.get("totals").and_then(serde_json::Value::as_object) {
        for (key, value) in totals {
            let count = value.as_u64().unwrap_or(0);
            println!("  {:<28} {}", key.replace('_', "-"), count);
        }
    }

    print_breakdown_section(summary, "retry_reasons", "Retry Reasons");
    print_breakdown_section(summary, "timeout_reasons", "Timeout Reasons");
    print_breakdown_section(summary, "duplicate_suppressions", "Duplicate Suppressions");
    print_breakdown_section(summary, "child_session_orphans", "Child Session Orphans");
    print_breakdown_section(summary, "workflow_phase_transitions", "Workflow Phase Transitions");
    print_breakdown_section(summary, "result_delivery", "Result Delivery");
    print_breakdown_section(summary, "session_replay", "Session Replay");
}

fn print_breakdown_section(summary: &serde_json::Value, key: &str, title: &str) {
    let Some(rows) = summary
        .get("breakdowns")
        .and_then(|value| value.get(key))
        .and_then(serde_json::Value::as_array)
    else {
        return;
    };

    if rows.is_empty() {
        return;
    }

    println!();
    println!("{}", title.cyan().bold());
    println!("{}", "─".repeat(60).dimmed());

    for row in rows {
        let Some(object) = row.as_object() else {
            continue;
        };
        let count = object
            .get("count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let dims = object
            .iter()
            .filter(|(name, _)| *name != "count")
            .map(|(name, value)| format!("{name}={}", value.as_str().unwrap_or("unknown")))
            .collect::<Vec<_>>()
            .join(", ");
        println!("  {:<48} {}", dims, count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_prefers_cli_then_env_then_default() {
        assert_eq!(resolve_base_url(Some("https://example.com".into())), "https://example.com");
        assert_eq!(resolve_base_url(None), "http://127.0.0.1:3000");
    }

    #[test]
    fn auth_token_filters_blank_values() {
        assert_eq!(resolve_auth_token(Some("token".into())), Some("token".into()));
        assert_eq!(resolve_auth_token(Some("   ".into())), None);
    }
}
