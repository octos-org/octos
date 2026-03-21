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
    /// Unique tunnel auth token (UUID v4).
    pub tunnel_token: String,
    /// Allocated SSH tunnel port on the VPS (6001–6999).
    pub ssh_port: u16,
    /// Local octos serve port on the tenant machine.
    #[serde(default = "default_local_port")]
    pub local_port: u16,
    /// Dashboard auth token for this tenant's octos serve instance.
    #[serde(default)]
    pub auth_token: String,
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

        // Restrict file permissions to owner-only (mode 0600) — contains tunnel token
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
/// `frps_token` is the shared frps master token (NOT the per-tenant tunnel_token).
pub fn render_frpc_config(
    tenant: &TenantConfig,
    frps_server: &str,
    frps_port: u16,
    frps_token: &str,
    tunnel_domain: &str,
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
    fn should_reject_empty_tenant_id() {
        assert!(validate_tenant_id("").is_err());
    }

    #[test]
    fn should_reject_uppercase_tenant_id() {
        assert!(validate_tenant_id("Alice").is_err());
    }

    #[test]
    fn should_reject_tenant_id_with_leading_hyphen() {
        assert!(validate_tenant_id("-alice").is_err());
    }

    #[test]
    fn should_reject_tenant_id_with_trailing_hyphen() {
        assert!(validate_tenant_id("alice-").is_err());
    }

    #[test]
    fn should_accept_valid_tenant_id() {
        assert!(validate_tenant_id("alice").is_ok());
        assert!(validate_tenant_id("alice-mini").is_ok());
        assert!(validate_tenant_id("bob-2").is_ok());
    }

    #[test]
    fn should_reject_path_traversal_ids() {
        assert!(validate_tenant_id("../etc").is_err());
        assert!(validate_tenant_id("..").is_err());
        assert!(validate_tenant_id("foo/bar").is_err());
        assert!(validate_tenant_id("foo\\bar").is_err());
        assert!(validate_tenant_id(".hidden").is_err());
    }

    #[test]
    fn should_reject_get_with_invalid_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = TenantStore::open(dir.path()).unwrap();
        assert!(store.get("../etc/passwd").is_err());
        assert!(store.get("..").is_err());
    }

    #[test]
    fn should_reject_delete_with_invalid_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = TenantStore::open(dir.path()).unwrap();
        assert!(store.delete("../etc/passwd").is_err());
    }

    #[test]
    fn should_reject_tenant_id_over_64_chars() {
        let long = "a".repeat(65);
        assert!(validate_tenant_id(&long).is_err());
    }

    #[test]
    fn should_render_frpc_config_with_all_placeholders() {
        let tenant = TenantConfig {
            id: "alice".into(),
            name: "Alice".into(),
            subdomain: "alice".into(),
            tunnel_token: "test-token-123".into(),
            ssh_port: 6001,
            local_port: 8080,
            auth_token: "test-auth-token".into(),
            status: TenantStatus::Pending,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let config = render_frpc_config(
            &tenant,
            "163.192.33.32",
            7000,
            "master-secret",
            "octos-cloud.org",
        );
        assert!(config.contains("serverAddr = \"163.192.33.32\""));
        assert!(config.contains("serverPort = 7000"));
        assert!(config.contains("auth.token = \"master-secret\""));
        // Must NOT contain the per-tenant token
        assert!(!config.contains("test-token-123"));
        assert!(config.contains("\"alice.octos-cloud.org\""));
        assert!(config.contains("localPort = 8080"));
        assert!(config.contains("remotePort = 6001"));
        // No unresolved placeholders
        assert!(!config.contains("{{"));
    }

    #[test]
    fn should_store_and_retrieve_tenant() {
        let dir = tempfile::tempdir().unwrap();
        let store = TenantStore::open(dir.path()).unwrap();

        let tenant = TenantConfig {
            id: "test".into(),
            name: "Test Tenant".into(),
            subdomain: "test".into(),
            tunnel_token: "tok-abc".into(),
            ssh_port: 6001,
            local_port: 8080,
            auth_token: "test-auth-token".into(),
            status: TenantStatus::Pending,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        store.save(&tenant).unwrap();

        let loaded = store.get("test").unwrap().expect("tenant should exist");
        assert_eq!(loaded.id, "test");
        assert_eq!(loaded.tunnel_token, "tok-abc");
        assert_eq!(loaded.ssh_port, 6001);
    }

    #[test]
    fn should_list_tenants_sorted_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let store = TenantStore::open(dir.path()).unwrap();

        for (id, name) in [("bob", "Bob"), ("alice", "Alice"), ("charlie", "Charlie")] {
            let tenant = TenantConfig {
                id: id.into(),
                name: name.into(),
                subdomain: id.into(),
                tunnel_token: format!("tok-{id}"),
                ssh_port: 6001 + id.len() as u16,
                local_port: 8080,
                auth_token: format!("auth-{id}"),
                status: TenantStatus::Pending,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            store.save(&tenant).unwrap();
        }

        let list = store.list().unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].name, "Alice");
        assert_eq!(list[1].name, "Bob");
        assert_eq!(list[2].name, "Charlie");
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
            status: TenantStatus::Pending,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.save(&tenant).unwrap();

        // Next allocation should skip 6001
        assert_eq!(store.next_ssh_port().unwrap(), SSH_PORT_START + 1);
    }

    #[test]
    fn should_delete_tenant() {
        let dir = tempfile::tempdir().unwrap();
        let store = TenantStore::open(dir.path()).unwrap();

        let tenant = TenantConfig {
            id: "del-me".into(),
            name: "Delete Me".into(),
            subdomain: "del-me".into(),
            tunnel_token: "tok".into(),
            ssh_port: 6001,
            local_port: 8080,
            auth_token: "test-auth-token".into(),
            status: TenantStatus::Pending,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        store.save(&tenant).unwrap();
        assert!(store.delete("del-me").unwrap());
        assert!(store.get("del-me").unwrap().is_none());
        // Double delete returns false
        assert!(!store.delete("del-me").unwrap());
    }
}
