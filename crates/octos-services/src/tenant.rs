//! Tunnel tenant management for self-hosted Mac Mini deployments.
//!
//! Each tenant represents a remote machine that connects to the VPS relay
//! via frp tunnel. Tenants are stored as individual JSON files in
//! `~/.octos/tenants/`.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr, bail};
use serde::{Deserialize, Serialize};

/// SSH port pool range for tunnel tenants.
pub const SSH_PORT_START: u16 = 6001;
pub const SSH_PORT_END: u16 = 6999;

/// A tunnel tenant — a remote machine accessible via frp tunnel.
///
/// `id`, `name`, and `subdomain` are typically the same value at creation.
/// They're separate fields to allow future flexibility: `name` for display,
/// `subdomain` for DNS routing (could differ from `id` if custom domains are added).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TenantConfig {
    /// Unique identifier (slug: lowercase alphanumeric + hyphens).
    pub id: String,
    /// Display name (defaults to `id` at creation).
    pub name: String,
    /// Subdomain for tunnel routing (defaults to `id` at creation).
    pub subdomain: String,
    /// Per-tenant tunnel auth token (UUID). Used as `auth.token` in frpc.toml.
    /// The frps plugin verifies `md5(tunnel_token + timestamp)` during Login.
    #[serde(default)]
    pub tunnel_token: String,
    /// Allocated SSH tunnel port on the VPS (6001–6999).
    pub ssh_port: u16,
    /// Local octos serve port on the tenant machine.
    #[serde(default = "default_local_port")]
    pub local_port: u16,
    /// Dashboard auth token for this tenant's octos serve instance.
    #[serde(default)]
    pub auth_token: String,
    /// User ID of the owner who registered this tenant (empty for admin-created).
    #[serde(default, alias = "email")]
    pub owner: String,
    /// Current tunnel status.
    #[serde(default)]
    pub status: TenantStatus,
    /// When this tenant was created.
    pub created_at: DateTime<Utc>,
    /// When this tenant was last modified.
    pub updated_at: DateTime<Utc>,
}

fn default_local_port() -> u16 {
    8080
}

/// Tenant tunnel connection status.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TenantStatus {
    /// Tenant registered but not yet connected.
    #[default]
    Pending,
    /// Tunnel is active and connected.
    Online,
    /// Tunnel was previously connected but is now disconnected.
    Offline,
}

impl std::fmt::Display for TenantStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Online => write!(f, "online"),
            Self::Offline => write!(f, "offline"),
        }
    }
}

/// Persistent store for tunnel tenants (JSON files in a directory).
pub struct TenantStore {
    tenants_dir: PathBuf,
}

impl TenantStore {
    /// Open (or create) the tenant store at `data_dir/tenants/`.
    pub fn open(data_dir: &Path) -> Result<Self> {
        let tenants_dir = data_dir.join("tenants");
        std::fs::create_dir_all(&tenants_dir)
            .wrap_err_with(|| format!("failed to create tenants dir: {}", tenants_dir.display()))?;
        Ok(Self { tenants_dir })
    }

    /// List all tenants sorted by name.
    pub fn list(&self) -> Result<Vec<TenantConfig>> {
        let mut tenants = Vec::new();
        let entries = match std::fs::read_dir(&self.tenants_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(tenants),
            Err(e) => return Err(e).wrap_err("failed to read tenants directory"),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                match std::fs::read_to_string(&path) {
                    Ok(content) => match serde_json::from_str::<TenantConfig>(&content) {
                        Ok(tenant) => tenants.push(tenant),
                        Err(e) => {
                            tracing::warn!(
                                path = %path.display(),
                                error = %e,
                                "skipping invalid tenant"
                            );
                        }
                    },
                    Err(e) => {
                        tracing::warn!(
                            path = %path.display(),
                            error = %e,
                            "failed to read tenant file"
                        );
                    }
                }
            }
        }
        tenants.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(tenants)
    }

    /// Get a tenant by ID.
    pub fn get(&self, id: &str) -> Result<Option<TenantConfig>> {
        validate_tenant_id(id)?;
        let path = self.tenant_path(id);
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)
            .wrap_err_with(|| format!("failed to read tenant: {id}"))?;
        let tenant = serde_json::from_str(&content)
            .wrap_err_with(|| format!("failed to parse tenant: {id}"))?;
        Ok(Some(tenant))
    }

    /// Save a tenant (create or update). Uses atomic write-then-rename.
    pub fn save(&self, tenant: &TenantConfig) -> Result<()> {
        validate_tenant_id(&tenant.id)?;
        let path = self.tenant_path(&tenant.id);
        let tmp_path = path.with_extension("json.tmp");
        let content =
            serde_json::to_string_pretty(tenant).wrap_err("failed to serialize tenant")?;

        // Write to temp file then rename for crash safety
        std::fs::write(&tmp_path, &content)
            .wrap_err_with(|| format!("failed to write tenant: {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, &path)
            .wrap_err_with(|| format!("failed to rename tenant: {}", path.display()))?;

        // Restrict file permissions to owner-only (mode 0600) — contains tenant auth metadata
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            {
                tracing::warn!(path = %path.display(), error = %e, "failed to set tenant file permissions");
            }
        }

        Ok(())
    }

    /// Delete a tenant by ID.
    pub fn delete(&self, id: &str) -> Result<bool> {
        validate_tenant_id(id)?;
        let path = self.tenant_path(id);
        if !path.exists() {
            return Ok(false);
        }
        std::fs::remove_file(&path).wrap_err_with(|| format!("failed to delete tenant: {id}"))?;
        Ok(true)
    }

    /// Allocate the next available SSH port from the pool.
    pub fn next_ssh_port(&self) -> Result<u16> {
        let tenants = self.list()?;
        let used: std::collections::HashSet<u16> = tenants.iter().map(|t| t.ssh_port).collect();
        for port in SSH_PORT_START..=SSH_PORT_END {
            if !used.contains(&port) {
                return Ok(port);
            }
        }
        bail!("SSH port pool exhausted ({SSH_PORT_START}–{SSH_PORT_END})")
    }

    /// Find a tenant by its tunnel auth token.
    pub fn find_by_tunnel_token(&self, token: &str) -> Result<Option<TenantConfig>> {
        let all = self.list()?;
        Ok(all.into_iter().find(|t| t.tunnel_token == token))
    }

    /// Find a tenant by assigned subdomain.
    pub fn find_by_subdomain(&self, subdomain: &str) -> Result<Option<TenantConfig>> {
        let all = self.list()?;
        Ok(all.into_iter().find(|t| t.subdomain == subdomain))
    }

    /// Find a tenant by allocated SSH remote port.
    pub fn find_by_ssh_port(&self, port: u16) -> Result<Option<TenantConfig>> {
        let all = self.list()?;
        Ok(all.into_iter().find(|t| t.ssh_port == port))
    }

    /// Find tenant(s) belonging to a user.
    ///
    /// Checks the `owner` field against all provided identifiers (user ID,
    /// email, etc.) to handle legacy tenants that stored the full email
    /// address instead of the user ID slug.
    pub fn find_by_owner(&self, identifiers: &[&str]) -> Result<Vec<TenantConfig>> {
        let all = self.list()?;
        Ok(all
            .into_iter()
            .filter(|t| !t.owner.is_empty() && identifiers.iter().any(|id| t.owner == *id))
            .collect())
    }

    fn tenant_path(&self, id: &str) -> PathBuf {
        self.tenants_dir.join(format!("{id}.json"))
    }
}

/// Validate a tenant ID (same rules as profile IDs).
fn validate_tenant_id(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("tenant ID cannot be empty");
    }
    if id.len() > 64 {
        bail!("tenant ID too long (max 64 chars)");
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        bail!("tenant ID must be lowercase alphanumeric + hyphens");
    }
    if id.starts_with('-') || id.ends_with('-') {
        bail!("tenant ID must not start or end with a hyphen");
    }
    Ok(())
}

/// Generate the frpc TOML config for a tenant by filling the template.
///
/// Uses the shared FRPS auth token for host-wide frps authentication.
pub fn render_frpc_config(
    tenant: &TenantConfig,
    frps_server: &str,
    frps_port: u16,
    tunnel_domain: &str,
    frps_token: &str,
) -> String {
    let template = include_str!("../../../scripts/frp/tenant-frpc.toml.template");
    template
        .replace("{{FRPS_SERVER}}", frps_server)
        .replace("{{FRPS_PORT}}", &frps_port.to_string())
        .replace("{{FRPS_TOKEN}}", frps_token)
        .replace("{{SUBDOMAIN}}", &tenant.subdomain)
        .replace("{{LOCAL_PORT}}", &tenant.local_port.to_string())
        .replace("{{SSH_REMOTE_PORT}}", &tenant.ssh_port.to_string())
        .replace("{{TUNNEL_DOMAIN}}", tunnel_domain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_reject_path_traversal_ids() {
        assert!(validate_tenant_id("../etc").is_err());
        assert!(validate_tenant_id("..").is_err());
        assert!(validate_tenant_id("foo/bar").is_err());
        assert!(validate_tenant_id("foo\\bar").is_err());
        assert!(validate_tenant_id(".hidden").is_err());
    }

    #[test]
    fn should_allocate_next_ssh_port() {
        let dir = tempfile::tempdir().unwrap();
        let store = TenantStore::open(dir.path()).unwrap();

        // First allocation should be SSH_PORT_START
        assert_eq!(store.next_ssh_port().unwrap(), SSH_PORT_START);

        // Save a tenant with port 6001
        let tenant = TenantConfig {
            id: "first".into(),
            name: "First".into(),
            subdomain: "first".into(),
            tunnel_token: "tok".into(),
            ssh_port: SSH_PORT_START,
            local_port: 8080,
            auth_token: "test-auth".into(),
            owner: String::new(),
            status: TenantStatus::Pending,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.save(&tenant).unwrap();

        // Next allocation should skip 6001
        assert_eq!(store.next_ssh_port().unwrap(), SSH_PORT_START + 1);
    }
}
