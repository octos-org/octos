//! Durable, profile-scoped session permission selections.
//!
//! Legacy means "no record": only a missing file inherits the server default.
//! Corrupt, partial, misplaced, unreadable, or unwritable records are errors and
//! are remembered as such instead of falling back to wider permissions.

use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use octos_agent::ApprovalPolicy;
use octos_core::{
    SessionKey,
    ui_protocol::{PermissionNetworkPolicy, PermissionProfileMode, PermissionProfileSelection},
};
use serde::{Deserialize, Serialize};

const RECORD_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, Default)]
pub struct StoredSessionPermissionProfile {
    pub selection: PermissionProfileSelection,
    pub approval_policy: Option<ApprovalPolicy>,
}

#[derive(Debug, Clone)]
pub struct SessionPermissionError {
    pub kind: &'static str,
    pub detail: String,
}

impl SessionPermissionError {
    fn invalid(detail: impl Into<String>) -> Self {
        Self {
            kind: "session_permission_invalid",
            detail: detail.into(),
        }
    }

    fn io(detail: impl Into<String>) -> Self {
        Self {
            kind: "session_permission_io",
            detail: detail.into(),
        }
    }
}

type PermissionResult = Result<StoredSessionPermissionProfile, SessionPermissionError>;

#[derive(Default)]
pub struct SessionPermissionProfileStore {
    entries: Mutex<HashMap<(Option<String>, SessionKey), PermissionResult>>,
}

impl SessionPermissionProfileStore {
    pub fn get_state_explicit(
        &self,
        session_id: &SessionKey,
        authorized_profile_id: Option<&str>,
    ) -> Option<PermissionResult> {
        let profile_id = session_id
            .profile_id()
            .or(authorized_profile_id)
            .map(ToOwned::to_owned);
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&(profile_id, session_id.clone()))
            .cloned()
    }

    pub fn set(
        &self,
        session_id: SessionKey,
        authorized_profile_id: Option<&str>,
        selection: PermissionProfileSelection,
        approval_policy: Option<ApprovalPolicy>,
    ) {
        let profile_id = session_id
            .profile_id()
            .or(authorized_profile_id)
            .map(ToOwned::to_owned);
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(
                (profile_id, session_id),
                Ok(StoredSessionPermissionProfile {
                    selection,
                    approval_policy,
                }),
            );
    }

    pub fn poison(
        &self,
        session_id: &SessionKey,
        authorized_profile_id: Option<&str>,
        error: SessionPermissionError,
    ) {
        let profile_id = session_id
            .profile_id()
            .or(authorized_profile_id)
            .map(ToOwned::to_owned);
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert((profile_id, session_id.clone()), Err(error));
    }

    pub fn remove(&self, session_id: &SessionKey, authorized_profile_id: Option<&str>) {
        let profile_id = session_id
            .profile_id()
            .or(authorized_profile_id)
            .map(ToOwned::to_owned);
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&(profile_id, session_id.clone()));
    }
}

pub fn session_permission_profiles() -> Arc<SessionPermissionProfileStore> {
    static STORE: OnceLock<Arc<SessionPermissionProfileStore>> = OnceLock::new();
    STORE
        .get_or_init(|| Arc::new(SessionPermissionProfileStore::default()))
        .clone()
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PermissionRecord {
    version: u32,
    profile_id: String,
    session_id: String,
    mode: String,
    network: String,
    approval_policy: String,
}

impl PermissionRecord {
    fn new(
        profile_id: Option<&str>,
        session_id: &SessionKey,
        state: StoredSessionPermissionProfile,
    ) -> Self {
        Self {
            version: RECORD_VERSION,
            profile_id: profile_id.unwrap_or("_main").to_owned(),
            session_id: session_id.0.clone(),
            mode: match state.selection.mode {
                PermissionProfileMode::ReadOnly => "read_only",
                PermissionProfileMode::WorkspaceWrite => "workspace_write",
                PermissionProfileMode::DangerFullAccess => "danger_full_access",
            }
            .into(),
            network: match state.selection.network {
                PermissionNetworkPolicy::Deny => "deny",
                PermissionNetworkPolicy::Allow => "allow",
            }
            .into(),
            approval_policy: match state.approval_policy {
                Some(ApprovalPolicy::Ask) => "on-request",
                Some(ApprovalPolicy::Never) => "never",
                None => "inherit",
            }
            .into(),
        }
    }

    fn validate(
        &self,
        data_dir: &Path,
        path: &Path,
    ) -> Result<StoredSessionPermissionProfile, SessionPermissionError> {
        if self.version != RECORD_VERSION {
            return Err(SessionPermissionError::invalid(format!(
                "unsupported permission record version {} in {}",
                self.version,
                path.display()
            )));
        }
        let session_id = SessionKey(self.session_id.clone());
        let record_profile = (self.profile_id != "_main").then_some(self.profile_id.as_str());
        let expected = permission_record_path(data_dir, record_profile, &session_id);
        if expected != path {
            return Err(SessionPermissionError::invalid(format!(
                "permission record identity/path mismatch: {}",
                path.display()
            )));
        }
        let mode = match self.mode.as_str() {
            "read_only" => PermissionProfileMode::ReadOnly,
            "workspace_write" => PermissionProfileMode::WorkspaceWrite,
            "danger_full_access" => PermissionProfileMode::DangerFullAccess,
            _ => {
                return Err(SessionPermissionError::invalid(format!(
                    "invalid permission mode in {}",
                    path.display()
                )));
            }
        };
        let network = match self.network.as_str() {
            "deny" => PermissionNetworkPolicy::Deny,
            "allow" => PermissionNetworkPolicy::Allow,
            _ => {
                return Err(SessionPermissionError::invalid(format!(
                    "invalid permission network in {}",
                    path.display()
                )));
            }
        };
        let approval_policy = match self.approval_policy.as_str() {
            "inherit" => None,
            "on-request" => Some(ApprovalPolicy::Ask),
            "never" => Some(ApprovalPolicy::Never),
            _ => {
                return Err(SessionPermissionError::invalid(format!(
                    "invalid approval policy in {}",
                    path.display()
                )));
            }
        };
        Ok(StoredSessionPermissionProfile {
            selection: PermissionProfileSelection { mode, network },
            approval_policy,
        })
    }
}

pub fn permission_record_path(
    data_dir: &Path,
    profile_id: Option<&str>,
    session_id: &SessionKey,
) -> PathBuf {
    let profile = profile_id
        .or_else(|| session_id.profile_id())
        .unwrap_or("_main");
    let base = octos_bus::session::encode_path_component(session_id.base_key());
    let topic = octos_bus::session::encode_path_component(session_id.topic().unwrap_or("default"));
    data_dir
        .join("session-permissions")
        .join(octos_bus::session::encode_path_component(profile))
        .join(base)
        .join(format!("{topic}.json"))
}

fn read_record(
    data_dir: &Path,
    profile_id: Option<&str>,
    session_id: &SessionKey,
) -> Result<Option<StoredSessionPermissionProfile>, SessionPermissionError> {
    let path = permission_record_path(data_dir, profile_id, session_id);
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(SessionPermissionError::io(format!(
                "read {}: {error}",
                path.display()
            )));
        }
    };
    let record: PermissionRecord = serde_json::from_str(&contents).map_err(|error| {
        SessionPermissionError::invalid(format!("parse {}: {error}", path.display()))
    })?;
    if record.profile_id
        != profile_id
            .or_else(|| session_id.profile_id())
            .unwrap_or("_main")
        || record.session_id != session_id.0
    {
        return Err(SessionPermissionError::invalid(format!(
            "permission record identity mismatch: {}",
            path.display()
        )));
    }
    record.validate(data_dir, &path).map(Some)
}

pub fn load_session_permission(
    data_dir: &Path,
    profile_id: Option<&str>,
    session_id: &SessionKey,
) -> Result<Option<StoredSessionPermissionProfile>, SessionPermissionError> {
    let store = session_permission_profiles();
    match read_record(data_dir, profile_id, session_id) {
        Ok(Some(state)) => {
            store.set(
                session_id.clone(),
                profile_id,
                state.selection,
                state.approval_policy,
            );
            Ok(Some(state))
        }
        Ok(None) => Ok(None),
        Err(error) => {
            store.poison(session_id, profile_id, error.clone());
            Err(error)
        }
    }
}

pub fn persist_session_permission(
    data_dir: &Path,
    profile_id: Option<&str>,
    session_id: &SessionKey,
    state: StoredSessionPermissionProfile,
) -> Result<(), SessionPermissionError> {
    let path = permission_record_path(data_dir, profile_id, session_id);
    let result: Result<(), SessionPermissionError> = (|| {
        let parent = path
            .parent()
            .ok_or_else(|| SessionPermissionError::invalid("permission record has no parent"))?;
        std::fs::create_dir_all(parent).map_err(|error| {
            SessionPermissionError::io(format!("create {}: {error}", parent.display()))
        })?;
        let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|error| {
            SessionPermissionError::io(format!(
                "create temporary record in {}: {error}",
                parent.display()
            ))
        })?;
        let record = PermissionRecord::new(
            profile_id.or_else(|| session_id.profile_id()),
            session_id,
            state,
        );
        serde_json::to_writer_pretty(&mut file, &record).map_err(|error| {
            SessionPermissionError::io(format!("encode {}: {error}", path.display()))
        })?;
        file.write_all(b"\n")
            .and_then(|_| file.as_file().sync_all())
            .map_err(|error| {
                SessionPermissionError::io(format!("write {}: {error}", path.display()))
            })?;
        file.persist(&path).map_err(|error| {
            SessionPermissionError::io(format!("replace {}: {error}", path.display()))
        })?;
        #[cfg(unix)]
        {
            let directory = std::fs::File::open(parent).map_err(|error| {
                SessionPermissionError::io(format!("open {} for fsync: {error}", parent.display()))
            })?;
            directory.sync_all().map_err(|error| {
                SessionPermissionError::io(format!("fsync {}: {error}", parent.display()))
            })?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            session_permission_profiles().set(
                session_id.clone(),
                profile_id,
                state.selection,
                state.approval_policy,
            );
            Ok(())
        }
        Err(error) => {
            session_permission_profiles().poison(session_id, profile_id, error.clone());
            Err(error)
        }
    }
}

pub fn clear_session_permission(
    data_dir: &Path,
    profile_id: Option<&str>,
    session_id: &SessionKey,
) -> Result<(), SessionPermissionError> {
    let path = permission_record_path(data_dir, profile_id, session_id);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            let error = SessionPermissionError::io(format!("remove {}: {error}", path.display()));
            session_permission_profiles().poison(session_id, profile_id, error.clone());
            return Err(error);
        }
    }
    session_permission_profiles().remove(session_id, profile_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_clear_durable_permission_without_resurrecting_selection() {
        let directory = tempfile::tempdir().unwrap();
        let session_id = SessionKey::with_profile("unit-clear", "api", "deleted");
        let state = StoredSessionPermissionProfile {
            selection: PermissionProfileSelection {
                mode: PermissionProfileMode::ReadOnly,
                network: PermissionNetworkPolicy::Deny,
            },
            approval_policy: Some(ApprovalPolicy::Never),
        };
        persist_session_permission(directory.path(), Some("unit-clear"), &session_id, state)
            .unwrap();
        assert!(permission_record_path(directory.path(), Some("unit-clear"), &session_id).exists());
        clear_session_permission(directory.path(), Some("unit-clear"), &session_id).unwrap();
        assert!(
            !permission_record_path(directory.path(), Some("unit-clear"), &session_id).exists()
        );
        assert!(
            session_permission_profiles()
                .get_state_explicit(&session_id, Some("unit-clear"))
                .is_none()
        );
    }
}
