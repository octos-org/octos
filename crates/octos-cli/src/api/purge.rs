//! Tenant purge cascade — single operation to fully remove a profile,
//! its user record, its tenant record, and all associated data so the same
//! email and node name can be re-registered cleanly.
//!
//! See `docs/plans/2026-04-10-purge-tenant-design.md` for the design.

use std::sync::Arc;

use serde::Serialize;

use crate::api::AppState;

/// Outcome of a purge operation. Returned to the caller as JSON or
/// printed by the CLI script.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct PurgeReport {
    pub profile_id: String,
    pub user_email: Option<String>,
    pub tenant_id: Option<String>,
    pub node_name: Option<String>,
    pub port_released: Option<u16>,
    pub files_removed: Vec<String>,
    pub bytes_freed: u64,
}

/// Cascade-purge a profile and everything bound to it.
///
/// Returns:
/// - `Ok(Some(report))` if the profile existed and was purged.
/// - `Ok(None)` if the profile was not found.
/// - `Err(_)` for unexpected storage errors.
///
/// Order (each step fault-tolerant — logs a warning instead of failing
/// when a record is already missing, so the function is idempotent on
/// partially-cleaned state):
/// 1. Stop the gateway process.
/// 2. Cascade sub-accounts (stop, remove data dir, delete profile JSON).
/// 3. Delete profile JSON + data directory.
/// 4. Delete user record.
/// 5. Find and delete the tenant record (releases SSH port implicitly).
pub async fn purge_by_profile_id(
    _state: &Arc<AppState>,
    _profile_id: &str,
) -> eyre::Result<Option<PurgeReport>> {
    todo!("implement in task 3")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::{ProfileStore, UserProfile};
    use crate::tenant::{TenantConfig, TenantStatus, TenantStore};
    use crate::user_store::{User, UserRole, UserStore};
    use std::sync::Arc;
    use tempfile::TempDir;

    /// Build an `AppState` with tempdir-backed stores. Returns the temp dir
    /// (must be kept alive for the test) and the state.
    fn build_test_state() -> (TempDir, Arc<AppState>) {
        let temp = TempDir::new().expect("tempdir");
        let data_dir = temp.path();
        let profile_store = Arc::new(ProfileStore::open(data_dir).expect("profile store"));
        let user_store = Arc::new(UserStore::open(data_dir).expect("user store"));
        let tenant_store = Arc::new(TenantStore::open(data_dir).expect("tenant store"));

        let state = Arc::new(AppState {
            profile_store: Some(profile_store),
            user_store: Some(user_store),
            tenant_store: Some(tenant_store),
            process_manager: None,
            ..AppState::empty_for_tests()
        });

        (temp, state)
    }

    fn make_profile(id: &str) -> UserProfile {
        UserProfile {
            id: id.to_string(),
            name: format!("Test {id}"),
            enabled: true,
            data_dir: None,
            parent_id: None,
            config: Default::default(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn make_user(id: &str, email: &str) -> User {
        User {
            id: id.to_string(),
            email: email.to_string(),
            name: format!("Test {id}"),
            role: UserRole::User,
            created_at: chrono::Utc::now(),
            last_login_at: None,
        }
    }

    fn make_tenant(id: &str, owner: &str, subdomain: &str, ssh_port: u16) -> TenantConfig {
        TenantConfig {
            id: id.to_string(),
            name: subdomain.to_string(),
            subdomain: subdomain.to_string(),
            tunnel_token: String::new(),
            ssh_port,
            local_port: 8080,
            auth_token: format!("auth-{id}"),
            owner: owner.to_string(),
            status: TenantStatus::Pending,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn should_purge_profile_user_tenant_and_data_dir_when_all_present() {
        let (temp, state) = build_test_state();
        let pid = "alice";

        // Set up: profile + user + tenant + data dir with content
        state
            .profile_store
            .as_ref()
            .unwrap()
            .save(&make_profile(pid))
            .unwrap();
        state
            .user_store
            .as_ref()
            .unwrap()
            .save(&make_user(pid, "alice@example.com"))
            .unwrap();
        state
            .tenant_store
            .as_ref()
            .unwrap()
            .save(&make_tenant("t-1", pid, "alice-mac", 6042))
            .unwrap();

        // Drop a fake file in the data dir to verify rm -rf later
        let profile = state
            .profile_store
            .as_ref()
            .unwrap()
            .get(pid)
            .unwrap()
            .unwrap();
        let data_dir = state
            .profile_store
            .as_ref()
            .unwrap()
            .resolve_data_dir(&profile);
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(data_dir.join("episodes.redb"), b"fake-data").unwrap();

        // Act
        let report = purge_by_profile_id(&state, pid)
            .await
            .expect("purge")
            .expect("Some(report)");

        // Assert: report shape
        assert_eq!(report.profile_id, pid);
        assert_eq!(report.user_email.as_deref(), Some("alice@example.com"));
        assert_eq!(report.tenant_id.as_deref(), Some("t-1"));
        assert_eq!(report.node_name.as_deref(), Some("alice-mac"));
        assert_eq!(report.port_released, Some(6042));
        assert!(report.bytes_freed > 0);

        // Assert: filesystem
        assert!(
            state
                .profile_store
                .as_ref()
                .unwrap()
                .get(pid)
                .unwrap()
                .is_none()
        );
        assert!(state.user_store.as_ref().unwrap().get(pid).unwrap().is_none());
        assert!(
            state
                .tenant_store
                .as_ref()
                .unwrap()
                .get("t-1")
                .unwrap()
                .is_none()
        );
        assert!(!data_dir.exists());

        drop(temp);
    }
}
