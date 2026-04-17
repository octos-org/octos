//! Admin commands for tenant, tunnel, and operator management.

use std::fmt::Write as _;
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
    print!("{}", render_operator_summary(base_url, summary));
}

fn render_operator_summary(base_url: &str, summary: &serde_json::Value) -> String {
    let mut output = String::new();
    writeln!(&mut output, "{}", "octos Operator Summary".cyan().bold()).unwrap();
    writeln!(&mut output, "{}", "═".repeat(60)).unwrap();
    writeln!(&mut output, "{}: {}", "Base URL".green(), base_url).unwrap();
    writeln!(
        &mut output,
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
    )
    .unwrap();

    render_collection_section(&mut output, summary);
    render_source_activity_section(&mut output, summary);
    render_totals_section(&mut output, summary);
    render_breakdown_section(&mut output, summary, "retry_reasons", "Retry Reasons");
    render_breakdown_section(&mut output, summary, "timeout_reasons", "Timeout Reasons");
    render_breakdown_section(
        &mut output,
        summary,
        "duplicate_suppressions",
        "Duplicate Suppressions",
    );
    render_breakdown_section(
        &mut output,
        summary,
        "child_session_orphans",
        "Child Session Orphans",
    );
    render_breakdown_section(
        &mut output,
        summary,
        "workflow_phase_transitions",
        "Workflow Phase Transitions",
    );
    render_breakdown_section(&mut output, summary, "result_delivery", "Result Delivery");
    render_breakdown_section(&mut output, summary, "session_replay", "Session Replay");
    output
}

fn render_collection_section(output: &mut String, summary: &serde_json::Value) {
    let Some(collection) = summary
        .get("collection")
        .and_then(serde_json::Value::as_object)
    else {
        return;
    };

    output.push('\n');
    writeln!(output, "{}", "Coverage".cyan().bold()).unwrap();
    writeln!(output, "{}", "─".repeat(60).dimmed()).unwrap();
    for (label, key) in [
        ("running-gateways", "running_gateways"),
        ("gateways-with-api-port", "gateways_with_api_port"),
        ("gateways-missing-api-port", "gateways_missing_api_port"),
        ("scrape-failures", "scrape_failures"),
        ("sources-observed", "sources_observed"),
        ("sources-with-metrics", "sources_with_metrics"),
        ("sources-without-metrics", "sources_without_metrics"),
    ] {
        let value = collection
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        writeln!(output, "  {:<28} {}", label, value).unwrap();
    }

    let partial = collection
        .get("partial")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    writeln!(
        output,
        "  {:<28} {}",
        "collection",
        if partial {
            "partial".yellow().to_string()
        } else {
            "complete".green().to_string()
        }
    )
    .unwrap();
}

fn render_source_activity_section(output: &mut String, summary: &serde_json::Value) {
    let Some(rows) = summary.get("sources").and_then(serde_json::Value::as_array) else {
        return;
    };
    if rows.is_empty() {
        return;
    }

    output.push('\n');
    writeln!(output, "{}", "Runtime Sources".cyan().bold()).unwrap();
    writeln!(output, "{}", "─".repeat(60).dimmed()).unwrap();

    for row in rows {
        let Some(object) = row.as_object() else {
            continue;
        };
        let scope = object
            .get("scope")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let label = match object.get("profile_id").and_then(serde_json::Value::as_str) {
            Some(profile_id) => format!("{scope}:{profile_id}"),
            None => scope.to_string(),
        };
        let scrape_status = object
            .get("scrape_status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let sample_count = object
            .get("sample_count")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let pid = object
            .get("pid")
            .and_then(serde_json::Value::as_u64)
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string());
        let api_port = object
            .get("api_port")
            .and_then(serde_json::Value::as_u64)
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string());
        let uptime = object
            .get("uptime_secs")
            .and_then(serde_json::Value::as_i64)
            .map(format_uptime)
            .unwrap_or_else(|| "-".to_string());
        let totals = object
            .get("totals")
            .and_then(serde_json::Value::as_object)
            .map(render_source_totals)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "no operator counters".to_string());

        writeln!(
            output,
            "  {:<24} pid={} api={} scrape={} samples={} uptime={} {}",
            label, pid, api_port, scrape_status, sample_count, uptime, totals
        )
        .unwrap();

        if let Some(error) = object
            .get("scrape_error")
            .and_then(serde_json::Value::as_str)
            .filter(|error| !error.is_empty())
        {
            writeln!(output, "  {:<24} error={}", "", error).unwrap();
        }
    }
}

fn render_totals_section(output: &mut String, summary: &serde_json::Value) {
    output.push('\n');
    writeln!(output, "{}", "Totals".cyan().bold()).unwrap();
    writeln!(output, "{}", "─".repeat(60).dimmed()).unwrap();
    if let Some(totals) = summary.get("totals").and_then(serde_json::Value::as_object) {
        for (key, value) in totals {
            let count = value.as_u64().unwrap_or(0);
            writeln!(output, "  {:<28} {}", key.replace('_', "-"), count).unwrap();
        }
    }
}

fn render_breakdown_section(
    output: &mut String,
    summary: &serde_json::Value,
    key: &str,
    title: &str,
) {
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

    output.push('\n');
    writeln!(output, "{}", title.cyan().bold()).unwrap();
    writeln!(output, "{}", "─".repeat(60).dimmed()).unwrap();

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
        writeln!(output, "  {:<48} {}", dims, count).unwrap();
    }
}

fn render_source_totals(totals: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut active = totals
        .iter()
        .filter_map(|(name, value)| {
            let count = value.as_u64().unwrap_or(0);
            (count > 0).then(|| format!("{}={count}", name.replace('_', "-")))
        })
        .collect::<Vec<_>>();
    active.sort();
    active.join(", ")
}

fn format_uptime(uptime_secs: i64) -> String {
    if uptime_secs < 60 {
        return format!("{uptime_secs}s");
    }
    let minutes = uptime_secs / 60;
    let seconds = uptime_secs % 60;
    if minutes < 60 {
        return format!("{minutes}m{seconds:02}s");
    }
    let hours = minutes / 60;
    let rem_minutes = minutes % 60;
    format!("{hours}h{rem_minutes:02}m")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_prefers_cli_then_env_then_default() {
        assert_eq!(
            resolve_base_url(Some("https://example.com".into())),
            "https://example.com"
        );
        assert_eq!(resolve_base_url(None), "http://127.0.0.1:3000");
    }

    #[test]
    fn auth_token_filters_blank_values() {
        assert_eq!(
            resolve_auth_token(Some("token".into())),
            Some("token".into())
        );
        assert_eq!(resolve_auth_token(Some("   ".into())), None);
    }

    #[test]
    fn render_operator_summary_includes_collection_and_sources() {
        let summary = serde_json::json!({
            "available": true,
            "collection": {
                "running_gateways": 2,
                "gateways_with_api_port": 1,
                "gateways_missing_api_port": 1,
                "scrape_failures": 1,
                "sources_observed": 3,
                "sources_with_metrics": 2,
                "sources_without_metrics": 1,
                "partial": true
            },
            "sources": [
                {
                    "scope": "serve",
                    "scrape_status": "local",
                    "available": true,
                    "sample_count": 5,
                    "totals": {
                        "retries": 0,
                        "timeouts": 1
                    }
                },
                {
                    "scope": "gateway",
                    "profile_id": "alpha",
                    "scrape_status": "failed",
                    "scrape_error": "http 503",
                    "available": false,
                    "sample_count": 0,
                    "api_port": 51001,
                    "pid": 4242,
                    "uptime_secs": 125,
                    "totals": {
                        "retries": 0,
                        "timeouts": 0
                    }
                }
            ],
            "totals": {
                "retries": 3,
                "timeouts": 1
            },
            "breakdowns": {
                "retry_reasons": [
                    {"reason": "background_result_ack_timeout", "count": 3}
                ]
            }
        });

        let rendered = render_operator_summary("http://127.0.0.1:3000", &summary);

        assert!(rendered.contains("Coverage"));
        assert!(rendered.contains("Runtime Sources"));
        assert!(rendered.contains("gateway:alpha"));
        assert!(rendered.contains("error=http 503"));
        assert!(rendered.contains("collection"));
        assert!(rendered.contains("partial"));
        assert!(rendered.contains("timeouts=1"));
    }

    #[test]
    fn format_uptime_uses_short_human_units() {
        assert_eq!(format_uptime(42), "42s");
        assert_eq!(format_uptime(125), "2m05s");
        assert_eq!(format_uptime(3665), "1h01m");
    }
}
