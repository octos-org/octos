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

fn read_json<T: for<'de> serde::Deserialize<'de> + Default>(path: &Path) -> T {
    let Ok(bytes) = std::fs::read(path) else {
        return T::default();
    };
    if bytes.len() > 8 * 1024 * 1024 {
        tracing::warn!(path = %path.display(), "catalog file exceeds 8 MiB; ignoring it");
        return T::default();
    }
    serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        tracing::warn!(path = %path.display(), %error, "catalog file is invalid; ignoring it");
        T::default()
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

fn scoped_profile_ids(state: &AppState, profile_id: Option<&str>) -> Vec<String> {
    if let Some(profile_id) = profile_id {
        return vec![profile_id.to_owned()];
    }
    let mut ids: Vec<String> = state.profiles.keys().cloned().collect();
    if !ids.iter().any(|id| id == MAIN_PROFILE_ID) {
        ids.push(MAIN_PROFILE_ID.to_owned());
    }
    ids.sort();
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
    let mut file: WorkspaceFile = read_json(&workspace_path(&data_dir));
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
    import_known_workspaces(state, profile_id)?;
    let data_dir = profile_data_dir(state, profile_id)?;
    let file: WorkspaceFile = read_json(&workspace_path(&data_dir));
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
    let mut file: WorkspaceFile = read_json(&workspace_path(&data_dir));
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
    let mut file: SessionFile = read_json(&session_path(&data_dir));
    let now = chrono::Utc::now().timestamp_millis();
    let workspace_id = workspace.map(|workspace| workspace.workspace_id.clone());
    let storage_kind = if workspace.is_some() {
        "cwd".to_owned()
    } else {
        "profile_global".to_owned()
    };
    let record = if let Some(existing) = file
        .sessions
        .iter_mut()
        .find(|record| record.session_id == session_id.0 && record.workspace_id == workspace_id)
    {
        existing.updated_at_ms = now;
        if touch_open {
            existing.last_opened_at_ms = Some(now);
        }
        existing.active_turn = info.active_turn;
        if info.title.is_some() {
            existing.title = info.title.clone();
        }
        existing.clone()
    } else {
        let created = info
            .updated_at
            .as_deref()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.timestamp_millis())
            .unwrap_or(now);
        let record = SessionRefRecord {
            session_ref: format!("sr_{}", uuid::Uuid::now_v7().simple()),
            session_id: session_id.0.clone(),
            workspace_id: workspace_id.clone(),
            workspace: None,
            profile_id: profile_id.to_owned(),
            title: info.title.clone(),
            created_at_ms: created,
            updated_at_ms: now,
            last_opened_at_ms: Some(now),
            storage_kind,
            active_turn: info.active_turn,
        };
        file.sessions.push(record.clone());
        record
    };
    write_json(&session_path(&data_dir), &file)?;
    Ok(with_workspace(record, workspace))
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
        let data_dir = profile_data_dir(state, &profile_id)?;
        let file: SessionFile = read_json(&session_path(&data_dir));
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

pub fn catalog_records(
    state: &AppState,
    profile_id: &str,
    workspace_id: Option<&str>,
) -> Result<Vec<SessionRefRecord>, RpcError> {
    let data_dir = profile_data_dir(state, profile_id)?;
    let file: SessionFile = read_json(&session_path(&data_dir));
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
}
