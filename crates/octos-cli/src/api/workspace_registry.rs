//! UPCR-2026-060 — profile-scoped Workspace and SessionRef catalogs.
//!
//! Transcript storage is deliberately untouched. These two small JSON indexes
//! live beside the profile's existing metadata and are seeded only from known
//! session/workspace records, never by crawling the host for `.octos` folders.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use octos_core::ui_protocol::{RpcError, SessionRefRecord, WorkspaceSummary, rpc_error_codes};
use octos_core::{MAIN_PROFILE_ID, SessionKey};
use sha2::{Digest, Sha256};

use super::AppState;
use super::handlers::SessionInfo;

fn registry_error(code: i64, kind: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(code, message).with_data(serde_json::json!({ "kind": kind }))
}

fn store_lock(data_dir: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .entry(data_dir.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct WorkspaceFile {
    workspaces: Vec<WorkspaceSummary>,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct SessionFile {
    sessions: Vec<SessionRefRecord>,
}

fn workspace_path(data_dir: &Path) -> PathBuf {
    data_dir.join("workspaces.json")
}

fn session_path(data_dir: &Path) -> PathBuf {
    data_dir.join("session-catalog.json")
}

const MAX_CATALOG_BYTES: usize = 8 * 1024 * 1024;

fn read_json<T: for<'de> serde::Deserialize<'de> + Default>(path: &Path) -> Result<T, RpcError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(T::default()),
        Err(error) => {
            return Err(registry_error(
                rpc_error_codes::INTERNAL_ERROR,
                "workspace_registry_unavailable",
                format!("failed to read catalog {}: {error}", path.display()),
            ));
        }
    };
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(registry_error(
            rpc_error_codes::INTERNAL_ERROR,
            "workspace_registry_unavailable",
            format!("catalog {} exceeds 8 MiB", path.display()),
        ));
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        registry_error(
            rpc_error_codes::INTERNAL_ERROR,
            "workspace_registry_unavailable",
            format!("catalog {} is invalid: {error}", path.display()),
        )
    })
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), RpcError> {
    let parent = path.parent().ok_or_else(|| {
        registry_error(
            rpc_error_codes::INTERNAL_ERROR,
            "workspace_registry_unavailable",
            "catalog path has no parent",
        )
    })?;
    std::fs::create_dir_all(parent).map_err(|error| {
        registry_error(
            rpc_error_codes::INTERNAL_ERROR,
            "workspace_registry_unavailable",
            format!("failed to create catalog directory: {error}"),
        )
    })?;
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| {
        registry_error(
            rpc_error_codes::INTERNAL_ERROR,
            "workspace_registry_unavailable",
            format!("failed to serialize catalog: {error}"),
        )
    })?;
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err(registry_error(
            rpc_error_codes::INTERNAL_ERROR,
            "workspace_registry_unavailable",
            "refusing to write a catalog larger than 8 MiB",
        ));
    }
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|error| {
        registry_error(
            rpc_error_codes::INTERNAL_ERROR,
            "workspace_registry_unavailable",
            format!("failed to create catalog temporary file: {error}"),
        )
    })?;
    std::io::Write::write_all(&mut temp, &bytes).map_err(|error| {
        registry_error(
            rpc_error_codes::INTERNAL_ERROR,
            "workspace_registry_unavailable",
            format!("failed to write catalog: {error}"),
        )
    })?;
    temp.as_file().sync_all().map_err(|error| {
        registry_error(
            rpc_error_codes::INTERNAL_ERROR,
            "workspace_registry_unavailable",
            format!("failed to sync catalog: {error}"),
        )
    })?;
    temp.persist(path).map_err(|error| {
        registry_error(
            rpc_error_codes::INTERNAL_ERROR,
            "workspace_registry_unavailable",
            format!("failed to persist catalog: {}", error.error),
        )
    })?;
    Ok(())
}

fn profile_data_dir(state: &AppState, profile_id: &str) -> Result<PathBuf, RpcError> {
    if let Some(runtime) =
        super::ui_protocol_transport::resolve_session_profile_runtime(state, Some(profile_id))
    {
        return Ok(runtime.data_dir.clone());
    }
    super::handlers::resolve_profile_data_dir_by_id(state, profile_id).map_err(|_| {
        registry_error(
            rpc_error_codes::RESOURCE_NOT_FOUND,
            "profile_not_found",
            "profile runtime was not found",
        )
    })
}

/// The profile's data dir when it can host a registry at all: a live
/// runtime, or a profile-store entry. `None` when the profile is absent —
/// an admin with no `_main` has no `_main` registry, which must not abort
/// resolution or listing for the profiles that DO exist (#2890 follow-up).
fn try_profile_data_dir(state: &AppState, profile_id: &str) -> Option<PathBuf> {
    if let Some(runtime) =
        super::ui_protocol_transport::resolve_session_profile_runtime(state, Some(profile_id))
    {
        return Some(runtime.data_dir.clone());
    }
    super::handlers::resolve_profile_data_dir_by_id(state, profile_id).ok()
}

fn scoped_profile_ids(state: &AppState, profile_id: Option<&str>) -> Vec<String> {
    if let Some(profile_id) = profile_id {
        return vec![profile_id.to_owned()];
    }
    let mut ids: Vec<String> = state.profiles.keys().cloned().collect();
    // Profiles that exist only in the store (runtime not yet bootstrapped)
    // still own registries and refs; include them so an unscoped (admin)
    // lookup can resolve their sessions (#2890 follow-up).
    if let Some(store) = state.profile_store.as_ref()
        && let Ok(profiles) = store.list()
    {
        ids.extend(profiles.into_iter().map(|profile| profile.id));
    }
    if !ids.iter().any(|id| id == MAIN_PROFILE_ID) {
        ids.push(MAIN_PROFILE_ID.to_owned());
    }
    ids.sort();
    ids.dedup();
    ids
}

fn canonical_allowed_roots(state: &AppState) -> Vec<PathBuf> {
    state
        .workspace_allowed_roots
        .iter()
        .filter_map(|root| dunce::canonicalize(root).ok())
        .filter(|root| root.as_path() != Path::new("/"))
        .collect()
}

fn workspace_allowed(state: &AppState, path: &Path) -> bool {
    canonical_allowed_roots(state)
        .iter()
        .any(|root| path == root || path.starts_with(root))
}

fn workspace_id(path: &Path) -> String {
    let digest = Sha256::digest(path.to_string_lossy().as_bytes());
    format!(
        "ws_{:x}",
        &digest[..8]
            .iter()
            .fold(0u64, |acc, byte| { (acc << 8) | u64::from(*byte) })
    )
}

fn new_workspace(
    path: PathBuf,
    profile_id: &str,
    display_name: Option<String>,
) -> WorkspaceSummary {
    let now = chrono::Utc::now().timestamp_millis();
    let display_name = display_name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| {
            path.file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| path.to_string_lossy().to_string())
        });
    WorkspaceSummary {
        workspace_id: workspace_id(&path),
        canonical_path: path.to_string_lossy().to_string(),
        display_name,
        profile_ids: vec![profile_id.to_owned()],
        created_at_ms: now,
        last_opened_at_ms: Some(now),
        available: true,
        writable: true,
        archived: false,
    }
}

fn import_known_workspaces(state: &AppState, profile_id: &str) -> Result<(), RpcError> {
    let data_dir = profile_data_dir(state, profile_id)?;
    let lock = store_lock(&data_dir);
    let _guard = lock.lock().unwrap_or_else(|error| error.into_inner());
    let mut file: WorkspaceFile = read_json(&workspace_path(&data_dir))?;
    let mut changed = false;
    for root in crate::runtime::workspace_history::load(&data_dir).unwrap_or_default() {
        let Ok(canonical) = dunce::canonicalize(&root) else {
            continue;
        };
        if !workspace_allowed(state, &canonical) {
            continue;
        }
        if file
            .workspaces
            .iter()
            .any(|workspace| workspace.canonical_path == canonical.to_string_lossy())
        {
            continue;
        }
        file.workspaces
            .push(new_workspace(canonical, profile_id, None));
        changed = true;
    }
    if changed {
        file.workspaces
            .sort_by(|left, right| left.canonical_path.cmp(&right.canonical_path));
        write_json(&workspace_path(&data_dir), &file)?;
    }
    Ok(())
}

pub fn list_workspaces(
    state: &AppState,
    profile_id: &str,
) -> Result<Vec<WorkspaceSummary>, RpcError> {
    // No runtime and no store entry => no workspaces for this profile;
    // report an empty list instead of failing an admin's whole listing.
    if try_profile_data_dir(state, profile_id).is_none() {
        return Ok(Vec::new());
    }
    import_known_workspaces(state, profile_id)?;
    let data_dir = profile_data_dir(state, profile_id)?;
    let file: WorkspaceFile = read_json(&workspace_path(&data_dir))?;
    Ok(file
        .workspaces
        .into_iter()
        .filter(|workspace| workspace.profile_ids.iter().any(|id| id == profile_id))
        .map(|mut workspace| {
            let path = PathBuf::from(&workspace.canonical_path);
            workspace.available = path.is_dir();
            workspace.writable = std::fs::metadata(&path)
                .map(|metadata| !metadata.permissions().readonly())
                .unwrap_or(false);
            workspace
        })
        .collect())
}

pub fn register_workspace(
    state: &AppState,
    profile_id: &str,
    path: &str,
    display_name: Option<String>,
) -> Result<WorkspaceSummary, RpcError> {
    let canonical = dunce::canonicalize(path).map_err(|error| {
        registry_error(
            rpc_error_codes::INVALID_PARAMS,
            "workspace_path_invalid",
            format!("workspace path is not an existing directory: {error}"),
        )
    })?;
    if !canonical.is_dir() || !workspace_allowed(state, &canonical) {
        return Err(registry_error(
            rpc_error_codes::PERMISSION_DENIED,
            "workspace_root_denied",
            "workspace path is outside the operator-approved roots",
        ));
    }
    let data_dir = profile_data_dir(state, profile_id)?;
    let lock = store_lock(&data_dir);
    let _guard = lock.lock().unwrap_or_else(|error| error.into_inner());
    let mut file: WorkspaceFile = read_json(&workspace_path(&data_dir))?;
    let now = chrono::Utc::now().timestamp_millis();
    let summary = if let Some(existing) = file
        .workspaces
        .iter_mut()
        .find(|workspace| workspace.canonical_path == canonical.to_string_lossy().as_ref())
    {
        existing.last_opened_at_ms = Some(now);
        existing.available = true;
        existing.archived = false;
        if let Some(name) = display_name.filter(|name| !name.trim().is_empty()) {
            existing.display_name = name;
        }
        if !existing.profile_ids.iter().any(|id| id == profile_id) {
            existing.profile_ids.push(profile_id.to_owned());
        }
        existing.clone()
    } else {
        let summary = new_workspace(canonical, profile_id, display_name);
        file.workspaces.push(summary.clone());
        summary
    };
    write_json(&workspace_path(&data_dir), &file)?;
    Ok(summary)
}

pub fn get_workspace(
    state: &AppState,
    profile_id: &str,
    workspace_id: &str,
) -> Result<WorkspaceSummary, RpcError> {
    list_workspaces(state, profile_id)?
        .into_iter()
        .find(|workspace| workspace.workspace_id == workspace_id)
        .ok_or_else(|| {
            registry_error(
                rpc_error_codes::RESOURCE_NOT_FOUND,
                "workspace_not_found",
                "workspace was not found in the authorized profile",
            )
        })
}

pub fn resolve_workspace_for_create(
    state: &AppState,
    connection_profile_id: Option<&str>,
    workspace_id: &str,
    requested_profile_id: Option<&str>,
) -> Result<(String, WorkspaceSummary), RpcError> {
    if let (Some(connection), Some(requested)) = (connection_profile_id, requested_profile_id)
        && connection != requested
    {
        return Err(registry_error(
            rpc_error_codes::PERMISSION_DENIED,
            "auth_scope_violation",
            "profile_id is outside the authenticated profile",
        ));
    }
    let profile_id = connection_profile_id
        .or(requested_profile_id)
        .unwrap_or(MAIN_PROFILE_ID);
    let workspace = get_workspace(state, profile_id, workspace_id)?;
    if workspace.archived || !workspace.available || !workspace.writable {
        return Err(registry_error(
            rpc_error_codes::INVALID_PARAMS,
            "workspace_unavailable",
            "workspace is archived, unavailable, or not writable",
        ));
    }
    Ok((profile_id.to_owned(), workspace))
}

pub fn session_open_params_for_create(
    state: &AppState,
    connection_profile_id: Option<&str>,
    params: &octos_core::ui_protocol::SessionCreateParams,
) -> Result<octos_core::ui_protocol::SessionOpenParams, RpcError> {
    let (profile_id, workspace) = resolve_workspace_for_create(
        state,
        connection_profile_id,
        &params.workspace_id,
        params.profile_id.as_deref(),
    )?;
    Ok(octos_core::ui_protocol::SessionOpenParams {
        session_id: SessionKey::with_profile(
            &profile_id,
            "api",
            &uuid::Uuid::now_v7().simple().to_string(),
        ),
        session_ref: None,
        topic: None,
        profile_id: Some(profile_id),
        cwd: Some(workspace.canonical_path),
        sandbox: None,
        after: None,
        client_commands: None,
    })
}

#[derive(Clone, Copy)]
struct UpsertOptions {
    touch_open: bool,
    discovery: bool,
}

fn session_info_updated_at(info: &SessionInfo) -> Option<i64> {
    info.updated_at
        .as_deref()
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.timestamp_millis())
}

fn upsert_session_ref(
    file: &mut SessionFile,
    profile_id: &str,
    session_id: &SessionKey,
    workspace: Option<&WorkspaceSummary>,
    info: &SessionInfo,
    options: UpsertOptions,
) -> (SessionRefRecord, bool) {
    let now = chrono::Utc::now().timestamp_millis();
    let workspace_id = workspace.map(|workspace| workspace.workspace_id.clone());
    if let Some(existing) = file
        .sessions
        .iter_mut()
        .find(|record| record.session_id == session_id.0 && record.workspace_id == workspace_id)
    {
        let mut changed = false;
        if let Some(updated_at_ms) = session_info_updated_at(info)
            && existing.updated_at_ms != updated_at_ms
        {
            existing.updated_at_ms = updated_at_ms;
            changed = true;
        }
        if options.touch_open && existing.last_opened_at_ms != Some(now) {
            existing.last_opened_at_ms = Some(now);
            changed = true;
        }
        if !options.discovery && existing.active_turn != info.active_turn {
            existing.active_turn = info.active_turn;
            changed = true;
        }
        if info.title.is_some() && existing.title != info.title {
            existing.title = info.title.clone();
            changed = true;
        }
        return (existing.clone(), changed);
    }

    let created = session_info_updated_at(info).unwrap_or(now);
    let record = SessionRefRecord {
        session_ref: format!("sr_{}", uuid::Uuid::now_v7().simple()),
        session_id: session_id.0.clone(),
        workspace_id,
        workspace: None,
        profile_id: profile_id.to_owned(),
        title: info.title.clone(),
        created_at_ms: created,
        updated_at_ms: created,
        last_opened_at_ms: options.touch_open.then_some(now),
        storage_kind: if workspace.is_some() {
            "cwd".to_owned()
        } else {
            "profile_global".to_owned()
        },
        active_turn: !options.discovery && info.active_turn,
    };
    file.sessions.push(record.clone());
    (record, true)
}

pub fn ensure_session_ref(
    state: &AppState,
    profile_id: &str,
    session_id: &SessionKey,
    workspace: Option<&WorkspaceSummary>,
    info: &SessionInfo,
    touch_open: bool,
) -> Result<SessionRefRecord, RpcError> {
    let data_dir = profile_data_dir(state, profile_id)?;
    let lock = store_lock(&data_dir);
    let _guard = lock.lock().unwrap_or_else(|error| error.into_inner());
    let mut file: SessionFile = read_json(&session_path(&data_dir))?;
    let (record, changed) = upsert_session_ref(
        &mut file,
        profile_id,
        session_id,
        workspace,
        info,
        UpsertOptions {
            touch_open,
            discovery: false,
        },
    );
    if changed {
        write_json(&session_path(&data_dir), &file)?;
    }
    Ok(with_workspace(record, workspace))
}

/// Reconcile known legacy session metadata in one catalog transaction. Discovery
/// never counts as an open or live-turn persistence change; those are projected
/// onto response rows by the caller.
pub fn index_session_refs(
    state: &AppState,
    profile_id: &str,
    entries: Vec<(SessionKey, Option<WorkspaceSummary>, SessionInfo)>,
) -> Result<Vec<SessionRefRecord>, RpcError> {
    let data_dir = profile_data_dir(state, profile_id)?;
    let lock = store_lock(&data_dir);
    let _guard = lock.lock().unwrap_or_else(|error| error.into_inner());
    let mut file: SessionFile = read_json(&session_path(&data_dir))?;
    let mut changed = false;
    let mut records = Vec::with_capacity(entries.len());
    for (session_id, workspace, info) in entries {
        let (record, row_changed) = upsert_session_ref(
            &mut file,
            profile_id,
            &session_id,
            workspace.as_ref(),
            &info,
            UpsertOptions {
                touch_open: false,
                discovery: true,
            },
        );
        changed |= row_changed;
        records.push(with_workspace(record, workspace.as_ref()));
    }
    if changed {
        write_json(&session_path(&data_dir), &file)?;
    }
    Ok(records)
}

fn with_workspace(
    mut record: SessionRefRecord,
    workspace: Option<&WorkspaceSummary>,
) -> SessionRefRecord {
    record.workspace = workspace.cloned();
    record
}

pub fn resolve_session_ref(
    state: &AppState,
    connection_profile_id: Option<&str>,
    session_ref: &str,
) -> Result<SessionRefRecord, RpcError> {
    for profile_id in scoped_profile_ids(state, connection_profile_id) {
        // A profile with no runtime and no store entry has no session
        // registry; skip it rather than failing the whole lookup (an admin
        // without `_main` must still resolve another profile's refs).
        let Some(data_dir) = try_profile_data_dir(state, &profile_id) else {
            continue;
        };
        let file: SessionFile = read_json(&session_path(&data_dir))?;
        if let Some(record) = file
            .sessions
            .into_iter()
            .find(|record| record.session_ref == session_ref)
        {
            let workspace = match record.workspace_id.as_deref() {
                Some(workspace_id) => Some(get_workspace(state, &profile_id, workspace_id)?),
                None => None,
            };
            return Ok(with_workspace(record, workspace.as_ref()));
        }
    }
    Err(registry_error(
        rpc_error_codes::RESOURCE_NOT_FOUND,
        "session_not_found",
        "session_ref was not found in the authorized profile",
    ))
}

/// Remove every session_ref row for `session_id` from this profile's
/// registry, so `session/delete` leaves the old ref unresolvable instead of
/// resurrecting a deleted session on the next `session/open {session_ref}`.
/// No-op when the profile has no registry or the row is already gone.
pub fn remove_session_refs(
    state: &AppState,
    profile_id: &str,
    session_id: &str,
) -> Result<(), RpcError> {
    let Some(data_dir) = try_profile_data_dir(state, profile_id) else {
        return Ok(());
    };
    let lock = store_lock(&data_dir);
    let _guard = lock.lock().unwrap_or_else(|error| error.into_inner());
    let path = session_path(&data_dir);
    let mut file: SessionFile = read_json(&path)?;
    let before = file.sessions.len();
    // Match the (profile, session id) PAIR: profiles sharing one data dir
    // share this file, so filtering on the id alone would silently unbind
    // another profile's same-named session.
    file.sessions
        .retain(|record| !(record.session_id == session_id && record.profile_id == profile_id));
    if file.sessions.len() != before {
        write_json(&path, &file)?;
    }
    Ok(())
}

pub fn catalog_records(
    state: &AppState,
    profile_id: &str,
    workspace_id: Option<&str>,
) -> Result<Vec<SessionRefRecord>, RpcError> {
    let data_dir = profile_data_dir(state, profile_id)?;
    let file: SessionFile = read_json(&session_path(&data_dir))?;
    let workspaces = list_workspaces(state, profile_id)?;
    let mut records = Vec::new();
    for record in file.sessions {
        if workspace_id.is_some() && record.workspace_id.as_deref() != workspace_id {
            continue;
        }
        let workspace = record.workspace_id.as_deref().and_then(|id| {
            workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == id)
        });
        records.push(with_workspace(record, workspace));
    }
    records.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| left.session_ref.cmp(&right.session_ref))
    });
    Ok(records)
}

/// Paginate the catalog by its total sort key instead of a mutable row offset.
pub fn catalog_page(
    records: &[SessionRefRecord],
    cursor: Option<&str>,
    limit: usize,
) -> Result<(Vec<SessionRefRecord>, Option<String>), RpcError> {
    let after = cursor
        .map(|value| {
            let value = value
                .strip_prefix("v1:")
                .and_then(|value| value.split_once(':'))
                .ok_or_else(|| RpcError::invalid_params("session/list cursor is invalid"))?;
            let updated_at_ms = value
                .0
                .parse::<i64>()
                .map_err(|_| RpcError::invalid_params("session/list cursor is invalid"))?;
            if value.1.is_empty() {
                return Err(RpcError::invalid_params("session/list cursor is invalid"));
            }
            Ok((updated_at_ms, value.1))
        })
        .transpose()?;
    let limit = limit.max(1);
    let mut page: Vec<_> = records
        .iter()
        .filter(|record| {
            after.is_none_or(|(updated_at_ms, session_ref)| {
                record.updated_at_ms < updated_at_ms
                    || (record.updated_at_ms == updated_at_ms
                        && record.session_ref.as_str() > session_ref)
            })
        })
        .take(limit + 1)
        .cloned()
        .collect();
    let next_cursor = if page.len() > limit {
        page.truncate(limit);
        page.last()
            .map(|record| format!("v1:{}:{}", record.updated_at_ms, record.session_ref))
    } else {
        None
    };
    Ok((page, next_cursor))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_legacy_id_has_distinct_refs_per_workspace_and_unknown_ref_fails() {
        let temp = tempfile::tempdir().unwrap();
        let first_root = temp.path().join("first");
        let second_root = temp.path().join("second");
        std::fs::create_dir_all(&first_root).unwrap();
        std::fs::create_dir_all(&second_root).unwrap();
        let mut state = crate::api::AppState::empty_for_tests();
        state.workspace_allowed_roots = vec![temp.path().to_owned()];
        let store =
            std::sync::Arc::new(crate::profiles::ProfileStore::open_unified(temp.path()).unwrap());
        store
            .save(&crate::profiles::UserProfile {
                id: "main".to_owned(),
                name: "main".to_owned(),
                public_subdomain: None,
                enabled: true,
                data_dir: Some(
                    temp.path()
                        .join("profile-data")
                        .to_string_lossy()
                        .to_string(),
                ),
                parent_id: None,
                config: Default::default(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            })
            .unwrap();
        state.profile_store = Some(store);
        let first = register_workspace(&state, "main", first_root.to_str().unwrap(), None).unwrap();
        let second =
            register_workspace(&state, "main", second_root.to_str().unwrap(), None).unwrap();
        let info = SessionInfo {
            id: "api:shared".to_owned(),
            message_count: 2,
            title: Some("shared".to_owned()),
            updated_at: None,
            last_prompt: None,
            active_turn: false,
        };
        let first_ref = ensure_session_ref(
            &state,
            "main",
            &SessionKey("api:shared".to_owned()),
            Some(&first),
            &info,
            false,
        )
        .unwrap();
        let second_ref = ensure_session_ref(
            &state,
            "main",
            &SessionKey("api:shared".to_owned()),
            Some(&second),
            &info,
            false,
        )
        .unwrap();
        assert_ne!(first_ref.session_ref, second_ref.session_ref);
        assert_eq!(
            resolve_session_ref(&state, Some("main"), &first_ref.session_ref)
                .unwrap()
                .workspace_id,
            Some(first.workspace_id)
        );
        assert_eq!(
            resolve_session_ref(&state, Some("main"), &second_ref.session_ref)
                .unwrap()
                .workspace_id,
            Some(second.workspace_id)
        );
        let error = resolve_session_ref(&state, Some("main"), "sr_unknown").unwrap_err();
        assert_eq!(error.data.unwrap()["kind"], "session_not_found");
    }

    #[test]
    fn workspace_registration_rejects_outside_approved_roots() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let mut state = crate::api::AppState::empty_for_tests();
        state.workspace_allowed_roots = vec![temp.path().to_owned()];
        let store =
            std::sync::Arc::new(crate::profiles::ProfileStore::open_unified(temp.path()).unwrap());
        store
            .save(&crate::profiles::UserProfile {
                id: "main".to_owned(),
                name: "main".to_owned(),
                public_subdomain: None,
                enabled: true,
                data_dir: Some(
                    temp.path()
                        .join("profile-data")
                        .to_string_lossy()
                        .to_string(),
                ),
                parent_id: None,
                config: Default::default(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            })
            .unwrap();
        state.profile_store = Some(store);
        let error =
            register_workspace(&state, "main", outside.path().to_str().unwrap(), None).unwrap_err();
        assert_eq!(error.data.unwrap()["kind"], "workspace_root_denied");
    }

    #[test]
    fn workspace_ids_are_stable_and_opaque() {
        let root = Path::new("/srv/example");
        assert_eq!(workspace_id(root), workspace_id(root));
        assert_ne!(workspace_id(root), workspace_id(Path::new("/srv/other")));
        assert!(workspace_id(root).starts_with("ws_"));
    }

    fn catalog_test_state(root: &Path) -> AppState {
        let mut state = AppState::empty_for_tests();
        state.workspace_allowed_roots = vec![root.to_owned()];
        let store = std::sync::Arc::new(crate::profiles::ProfileStore::open_unified(root).unwrap());
        store
            .save(&crate::profiles::UserProfile {
                id: "main".to_owned(),
                name: "main".to_owned(),
                public_subdomain: None,
                enabled: true,
                data_dir: Some(root.join("profile-data").to_string_lossy().to_string()),
                parent_id: None,
                config: Default::default(),
                created_at: chrono::Utc::now(),
                updated_at: chrono::Utc::now(),
            })
            .unwrap();
        state.profile_store = Some(store);
        state
    }

    fn catalog_test_info(id: &str) -> SessionInfo {
        SessionInfo {
            id: id.to_owned(),
            message_count: 1,
            title: Some("title".to_owned()),
            updated_at: Some("2026-01-02T03:04:05Z".to_owned()),
            last_prompt: None,
            active_turn: false,
        }
    }

    #[test]
    fn reindexing_unchanged_sessions_preserves_timestamps_and_catalog_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let state = catalog_test_state(temp.path());
        let entries = || {
            vec![(
                SessionKey("api:stable".to_owned()),
                None,
                catalog_test_info("api:stable"),
            )]
        };
        let first = index_session_refs(&state, "main", entries())
            .unwrap()
            .remove(0);
        assert_eq!(first.updated_at_ms, first.created_at_ms);
        assert_eq!(first.last_opened_at_ms, None);
        let path = session_path(&temp.path().join("profile-data"));
        let before = std::fs::read(&path).unwrap();

        let mut live_info = catalog_test_info("api:stable");
        live_info.active_turn = true;
        let second = index_session_refs(
            &state,
            "main",
            vec![(SessionKey("api:stable".to_owned()), None, live_info)],
        )
        .unwrap()
        .remove(0);
        assert_eq!(second.updated_at_ms, first.updated_at_ms);
        assert_eq!(second.last_opened_at_ms, first.last_opened_at_ms);
        assert!(!second.active_turn);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    fn catalog_record(session_ref: &str, updated_at_ms: i64) -> SessionRefRecord {
        SessionRefRecord {
            session_ref: session_ref.to_owned(),
            session_id: format!("api:{session_ref}"),
            workspace_id: None,
            workspace: None,
            profile_id: "main".to_owned(),
            title: None,
            created_at_ms: updated_at_ms,
            updated_at_ms,
            last_opened_at_ms: None,
            storage_kind: "profile_global".to_owned(),
            active_turn: false,
        }
    }

    #[test]
    fn catalog_cursor_remains_stable_when_rows_are_inserted_between_pages() {
        let mut records = vec![
            catalog_record("sr_a", 300),
            catalog_record("sr_b", 200),
            catalog_record("sr_c", 200),
            catalog_record("sr_d", 100),
        ];
        let (first, cursor) = catalog_page(&records, None, 2).unwrap();
        assert_eq!(
            first
                .iter()
                .map(|row| row.session_ref.as_str())
                .collect::<Vec<_>>(),
            vec!["sr_a", "sr_b"]
        );
        let cursor = cursor.unwrap();

        records.push(catalog_record("sr_new", 250));
        records.sort_by(|left, right| {
            right
                .updated_at_ms
                .cmp(&left.updated_at_ms)
                .then_with(|| left.session_ref.cmp(&right.session_ref))
        });
        let (second, next) = catalog_page(&records, Some(&cursor), 2).unwrap();
        assert_eq!(
            second
                .iter()
                .map(|row| row.session_ref.as_str())
                .collect::<Vec<_>>(),
            vec!["sr_c", "sr_d"]
        );
        assert_eq!(next, None);
        assert!(catalog_page(&records, Some("not-a-cursor"), 2).is_err());
    }

    #[test]
    fn invalid_or_oversized_catalog_is_an_error_and_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("session-catalog.json");
        std::fs::write(&path, b"{invalid").unwrap();
        assert!(read_json::<SessionFile>(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{invalid");

        let oversized = vec![b' '; 8 * 1024 * 1024 + 1];
        std::fs::write(&path, &oversized).unwrap();
        assert!(read_json::<SessionFile>(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), oversized);

        let state = catalog_test_state(temp.path());
        let data_dir = temp.path().join("profile-data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let workspace_catalog = workspace_path(&data_dir);
        std::fs::write(&workspace_catalog, b"{invalid").unwrap();
        let root = temp.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        let error = register_workspace(&state, "main", root.to_str().unwrap(), None).unwrap_err();
        assert_eq!(
            error.data.unwrap()["kind"],
            "workspace_registry_unavailable"
        );
        assert_eq!(std::fs::read(workspace_catalog).unwrap(), b"{invalid");
    }
}
