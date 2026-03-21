//! Admin commands for tenant and tunnel management.

use clap::{Args, Subcommand};
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
    /// Create a new tunnel tenant (assigns subdomain, token, SSH port).
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
        /// Local octos serve port on the tenant machine.
        #[arg(long, default_value = "8080")]
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
        /// frps master auth token (or set FRPS_TOKEN env var).
        #[arg(long)]
        frps_token: Option<String>,
        /// Base domain for the tunnel.
        #[arg(long, default_value = "octos-cloud.org")]
        domain: String,
        /// frps server address.
        #[arg(long, default_value = "163.192.33.32")]
        server: String,
        /// frps control port.
        #[arg(long, default_value = "7000")]
        port: u16,
        /// Data directory override.
        #[arg(long)]
        data_dir: Option<std::path::PathBuf>,
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
                let tunnel_token = Uuid::new_v4().to_string();
                let auth_token = auth_token_arg.unwrap_or_else(|| {
                    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
                });
                let now = chrono::Utc::now();

                let tenant = TenantConfig {
                    id: name.clone(),
                    name: name.clone(),
                    subdomain: name.clone(),
                    tunnel_token: tunnel_token.clone(),
                    ssh_port,
                    local_port,
                    auth_token: auth_token.clone(),
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
                println!("  Tunnel tok: {}", tunnel_token);
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
                frps_token,
                domain,
                server,
                port,
                data_dir,
            } => {
                let data_dir = super::resolve_data_dir(data_dir)?;
                let store = TenantStore::open(&data_dir)?;

                let tenant = store
                    .get(&name)?
                    .ok_or_else(|| eyre::eyre!("tenant '{name}' not found"))?;

                let frps_token = frps_token
                    .or_else(|| std::env::var("FRPS_TOKEN").ok())
                    .ok_or_else(|| eyre::eyre!("--frps-token or FRPS_TOKEN env var required"))?;

                let config = render_frpc_config(&tenant, &server, port, &frps_token, &domain);
                println!("{config}");

                Ok(())
            }
        }
    }
}
