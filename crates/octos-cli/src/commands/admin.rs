//! Admin commands for tenant and tunnel management.

use clap::{Args, Subcommand};
use eyre::{Result, bail};
use uuid::Uuid;

use super::Executable;
use crate::tenant::{TenantConfig, TenantStore, TenantStatus, render_frpc_config};

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
                server,
                port,
                local_port,
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
                let now = chrono::Utc::now();

                let tenant = TenantConfig {
                    id: name.clone(),
                    name: name.clone(),
                    subdomain: name.clone(),
                    tunnel_token: tunnel_token.clone(),
                    ssh_port,
                    local_port,
                    status: TenantStatus::Pending,
                    created_at: now,
                    updated_at: now,
                };

                store.save(&tenant)?;

                println!("Tenant created:");
                println!("  ID:        {}", tenant.id);
                println!("  Subdomain: {}.{}", tenant.subdomain, domain);
                println!("  SSH port:  {}", tenant.ssh_port);
                println!("  Token:     {}", tunnel_token);
                println!();
                println!("Setup command for the tenant's Mac Mini:");
                println!(
                    "  curl -fsSL https://{domain}/setup | bash -s -- {} {}",
                    tenant.subdomain, tunnel_token
                );
                println!();
                println!("Or manually configure frpc:");
                let config = render_frpc_config(&tenant, &server, port, &domain);
                println!("{config}");

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
                data_dir,
            } => {
                let data_dir = super::resolve_data_dir(data_dir)?;
                let store = TenantStore::open(&data_dir)?;

                let tenant = store
                    .get(&name)?
                    .ok_or_else(|| eyre::eyre!("tenant '{name}' not found"))?;

                let config = render_frpc_config(&tenant, &server, port, &domain);
                println!("{config}");

                Ok(())
            }
        }
    }
}
