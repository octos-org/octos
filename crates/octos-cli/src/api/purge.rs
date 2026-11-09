//! Tenant purge cascade — single operation to fully remove a profile,
//! its user record, its tenant record, and all associated data so the same
//! email and node name can be re-registered cleanly.
//!
//! See `docs/plans/2026-04-10-purge-tenant-design.md` for the design.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
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
    state: &Arc<AppState>,
    profile_id: &str,
) -> eyre::Result<Option<PurgeReport>> {
    let Some(profile_store) = state.profile_store.as_ref() else {
        return Ok(None);
    };

    // Load profile first — if it doesn't exist, return Ok(None) so the
    // handler can map to 404. Storage errors propagate via `?`.
    let Some(profile) = profile_store.get(profile_id)? else {
        return Ok(None);
    };

    let mut report = PurgeReport {
        profile_id: profile_id.to_string(),
        user_email: None,
        tenant_id: None,
        node_name: None,
        port_released: None,
        files_removed: Vec::new(),
        bytes_freed: 0,
    };

    // 1. Stop the gateway (no-op if not running, no-op if no process manager)
    if let Some(pm) = state.process_manager.as_ref() {
        let _ = pm.stop(profile_id).await;
    }

    // 2. Cascade sub-accounts: stop, remove data dir, delete profile JSON
    let subs = profile_store
        .list_sub_accounts(profile_id)
        .unwrap_or_default();
    for sub in &subs {
        if let Some(pm) = state.process_manager.as_ref() {
            let _ = pm.stop(&sub.id).await;
        }
        let sub_data_dir = profile_store.resolve_data_dir(sub);
        if sub_data_dir.exists() {
            let bytes = dir_size(&sub_data_dir);
            if let Err(e) = std::fs::remove_dir_all(&sub_data_dir) {
                tracing::warn!(
                    sub_account = %sub.id,
                    dir = %sub_data_dir.display(),
                    error = %e,
                    "purge: failed to remove sub-account data dir"
                );
            } else {
                report.bytes_freed += bytes;
                report
                    .files_removed
                    .push(sub_data_dir.display().to_string());
            }
        }
        let _ = profile_store.delete(&sub.id);
        report
            .files_removed
            .push(format!("profiles/{}.json", sub.id));
    }

    // 3. Delete profile data dir + profile JSON
    let data_dir = profile_store.resolve_data_dir(&profile);
    if data_dir.exists() {
        let bytes = dir_size(&data_dir);
        if let Err(e) = std::fs::remove_dir_all(&data_dir) {
            tracing::warn!(profile = %profile_id, dir = %data_dir.display(), error = %e, "purge: failed to remove data dir");
        } else {
            report.bytes_freed += bytes;
            report.files_removed.push(data_dir.display().to_string());
        }
    }
    let _ = profile_store.delete(profile_id);
    report
        .files_removed
        .push(format!("profiles/{profile_id}.json"));
    // The purge's whole point is "the same tenant can re-register cleanly" —
    // its scoped keychain items must not outlive it either.
    crate::api::admin::release_deleted_profiles_keychain_items(
        profile_store,
        std::iter::once(&profile).chain(subs.iter()),
    );

    // 4. Delete user record (capture email first for the report)
    if let Some(us) = state.user_store.as_ref() {
        if let Ok(Some(user)) = us.get(profile_id) {
            report.user_email = Some(user.email);
        }
        if let Ok(true) = us.delete(profile_id) {
            report
                .files_removed
                .push(format!("users/{profile_id}.json"));
        }
    }

    // 5. Find and delete tenant record (releases SSH port implicitly)
    if let Some(ts) = state.tenant_store.as_ref() {
        let owner_keys: &[&str] = &[profile_id];
        if let Ok(tenants) = ts.find_by_owner(owner_keys) {
            for tenant in tenants {
                report.tenant_id = Some(tenant.id.clone());
                report.node_name = Some(tenant.subdomain.clone());
                report.port_released = Some(tenant.ssh_port);
                if let Ok(true) = ts.delete(&tenant.id) {
                    report
                        .files_removed
                        .push(format!("tenants/{}.json", tenant.id));
                }
            }
        }
    }

    tracing::info!(
        profile = %profile_id,
        files_removed = report.files_removed.len(),
        bytes_freed = report.bytes_freed,
        "purge complete"
    );

    Ok(Some(report))
}

/// POST /api/admin/profiles/{id}/purge — full cascade removal.
///
/// Maps the cascade result to HTTP status:
/// - `Ok(Some(report))` -> 200 with the report JSON
/// - `Ok(None)` -> 404 (profile not found, or admin stores not configured)
/// - `Err(_)` -> 500 (storage error)
pub async fn purge_profile_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<PurgeReport>, (StatusCode, String)> {
    if state.profile_store.is_none() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "admin not configured".into(),
        ));
    }

    match purge_by_profile_id(&state, &id).await {
        Ok(Some(report)) => Ok(Json(report)),
        Ok(None) => Err((StatusCode::NOT_FOUND, format!("profile '{id}' not found"))),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

/// POST /api/admin/profiles/by-node/{node_name}/purge — look up tenant by
/// subdomain, then purge its owner profile. Convenience for the CLI script.
pub async fn purge_by_node_handler(
    State(state): State<Arc<AppState>>,
    Path(node_name): Path<String>,
) -> Result<Json<PurgeReport>, (StatusCode, String)> {
    let ts = state.tenant_store.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "tenant store not configured".into(),
    ))?;

    let tenant = ts
        .find_by_subdomain(&node_name)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, format!("no node '{node_name}'")))?;

    purge_profile_handler(State(state), Path(tenant.owner)).await
}

/// Compute the recursive size of a directory in bytes. Returns 0 on any error.
fn dir_size(path: &std::path::Path) -> u64 {
    fn walk(path: &std::path::Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(path) else {
            return 0;
        };
        let mut total = 0u64;
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                total += walk(&entry.path());
            } else {
                total += meta.len();
            }
        }
        total
    }
    walk(path)
}
