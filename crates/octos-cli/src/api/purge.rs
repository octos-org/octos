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
