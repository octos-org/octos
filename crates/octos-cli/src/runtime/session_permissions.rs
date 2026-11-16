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

/// Omitted topic and explicit `default` share one transcript and one durable
/// permission identity. Other topics remain distinct.
fn canonical_session_identity(session_id: &SessionKey) -> SessionKey {
    if session_id.topic() == Some("default") {
        SessionKey(session_id.base_key().to_owned())
    } else {
        session_id.clone()
    }
}

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

type PermissionKey = (Option<String>, SessionKey);

#[derive(Default)]
pub struct SessionPermissionProfileStore {
    entries: Mutex<HashMap<PermissionKey, PermissionResult>>,
    generations: Mutex<HashMap<PermissionKey, u64>>,
}

impl SessionPermissionProfileStore {
    fn key(session_id: &SessionKey, authorized_profile_id: Option<&str>) -> PermissionKey {
        let session_id = canonical_session_identity(session_id);
        (
            session_id
                .profile_id()
                .or(authorized_profile_id)
                .map(ToOwned::to_owned),
            session_id,
        )
    }

    pub fn get_state_explicit(
        &self,
        session_id: &SessionKey,
        authorized_profile_id: Option<&str>,
    ) -> Option<PermissionResult> {
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&Self::key(session_id, authorized_profile_id))
            .cloned()
    }

    #[cfg(test)]
    fn generation(&self, key: &PermissionKey) -> u64 {
        self.generations
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(key)
            .copied()
            .unwrap_or(0)
    }

    fn begin_read(
        &self,
        session_id: &SessionKey,
        authorized_profile_id: Option<&str>,
    ) -> (PermissionKey, u64) {
        let key = Self::key(session_id, authorized_profile_id);
        let mut generations = self
            .generations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let generation = generations.get(&key).copied().unwrap_or(0);
        // Register even a first cold read so a concurrent base-scope DELETE can
        // fence its publication without knowing which topic is being loaded.
        generations.entry(key.clone()).or_insert(generation);
        (key, generation)
    }

    /// Publish a cold read only if no SET/DELETE/poison completed while the
    /// disk operation was in flight. The generation guard and entry write use a
    /// fixed generations→entries lock order.
    fn publish_read(
        &self,
        key: PermissionKey,
        expected_generation: u64,
        value: PermissionResult,
    ) -> bool {
        let mut generations = self
            .generations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if generations.get(&key).copied().unwrap_or(0) != expected_generation {
            return false;
        }
        // Keep the generation guard through publication so a writer cannot bump
        // between the comparison and the entry update.
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(key.clone(), value);
        generations.entry(key).or_insert(expected_generation);
        true
    }

    fn commit(&self, key: PermissionKey, value: Option<PermissionResult>) {
        let mut generations = self
            .generations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let next = generations
            .get(&key)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        generations.insert(key.clone(), next);
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match value {
            Some(value) => entries.insert(key, value),
            None => entries.remove(&key),
        };
    }

    pub fn set(
        &self,
        session_id: SessionKey,
        authorized_profile_id: Option<&str>,
        selection: PermissionProfileSelection,
        approval_policy: Option<ApprovalPolicy>,
    ) {
        self.commit(
            Self::key(&session_id, authorized_profile_id),
            Some(Ok(StoredSessionPermissionProfile {
                selection,
                approval_policy,
            })),
        );
    }

    pub fn poison(
        &self,
        session_id: &SessionKey,
        authorized_profile_id: Option<&str>,
        error: SessionPermissionError,
    ) {
        self.commit(
            Self::key(session_id, authorized_profile_id),
            Some(Err(error)),
        );
    }

    pub fn remove(&self, session_id: &SessionKey, authorized_profile_id: Option<&str>) {
        self.commit(Self::key(session_id, authorized_profile_id), None);
    }

    fn remove_scope(&self, session_id: &SessionKey, authorized_profile_id: Option<&str>) {
        let session_id = canonical_session_identity(session_id);
        let profile = session_id
            .profile_id()
            .or(authorized_profile_id)
            .map(ToOwned::to_owned);
        let required_key = Self::key(&session_id, authorized_profile_id);
        let mut generations = self
            .generations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut keys: Vec<PermissionKey> = generations
            .keys()
            .chain(entries.keys())
            .filter(|(entry_profile, entry_session)| {
                entry_profile == &profile
                    && entry_session.base_key() == session_id.base_key()
                    && (session_id.topic().is_none() || entry_session == &session_id)
            })
            .cloned()
            .collect();
        if !keys.contains(&required_key) {
            keys.push(required_key);
        }
        for key in keys {
            let next = generations
                .get(&key)
                .copied()
                .unwrap_or(0)
                .saturating_add(1);
            generations.insert(key.clone(), next);
            entries.remove(&key);
        }
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
        let session_id = canonical_session_identity(session_id);
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

fn bounded_permission_component(value: &str, suffix: &str) -> String {
    const MAX_FILE_NAME_BYTES: usize = 255;
    let encoded = octos_bus::session::encode_path_component(value);
    if encoded.len() + suffix.len() <= MAX_FILE_NAME_BYTES {
        return encoded;
    }
    // Existing valid paths keep their spelling. Only a component that cannot
    // exist on a 255-byte filesystem switches to the transcript's stable hash
    // encoding, so upgrades cannot silently orphan a valid restrictive record.
    let filename =
        octos_bus::SessionManager::bounded_session_file_name(&SessionKey(value.to_owned()));
    filename
        .strip_suffix(".jsonl")
        .unwrap_or(&filename)
        .to_owned()
}

pub fn permission_record_path(
    data_dir: &Path,
    profile_id: Option<&str>,
    session_id: &SessionKey,
) -> PathBuf {
    let profile = profile_id
        .or_else(|| session_id.profile_id())
        .unwrap_or("_main");
    let base = bounded_permission_component(session_id.base_key(), "");
    let topic = bounded_permission_component(session_id.topic().unwrap_or("default"), ".json");
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
    let session_id = canonical_session_identity(session_id);
    let path = permission_record_path(data_dir, profile_id, &session_id);
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
    let (key, generation) = store.begin_read(session_id, profile_id);
    match read_record(data_dir, profile_id, session_id) {
        Ok(Some(state)) => {
            if store.publish_read(key, generation, Ok(state)) {
                Ok(Some(state))
            } else {
                Ok(None)
            }
        }
        Ok(None) => Ok(None),
        Err(error) => {
            if store.publish_read(key, generation, Err(error.clone())) {
                Err(error)
            } else {
                Ok(None)
            }
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

/// Clear the complete durable scope of a base-session deletion. Sidecars are
/// enumerated independently of transcript existence, so a retry after the
/// gateway already removed every topic still cannot resurrect old permissions.
pub fn clear_session_permission_scope(
    data_dir: &Path,
    profile_id: Option<&str>,
    session_id: &SessionKey,
) -> Result<(), SessionPermissionError> {
    let session_id = canonical_session_identity(session_id);
    if session_id.topic().is_some() {
        return clear_session_permission(data_dir, profile_id, &session_id);
    }
    let base_record = permission_record_path(data_dir, profile_id, &session_id);
    let base_dir = base_record
        .parent()
        .ok_or_else(|| {
            SessionPermissionError::invalid("permission base record has no parent directory")
        })?
        .to_path_buf();
    let result = (|| {
        let entries = match std::fs::read_dir(&base_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(SessionPermissionError::io(format!(
                    "enumerate {}: {error}",
                    base_dir.display()
                )));
            }
        };
        for entry in entries {
            let entry = entry.map_err(|error| {
                SessionPermissionError::io(format!("enumerate {}: {error}", base_dir.display()))
            })?;
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "json") {
                continue;
            }
            if !entry
                .file_type()
                .map_err(|error| SessionPermissionError::io(error.to_string()))?
                .is_file()
            {
                return Err(SessionPermissionError::invalid(format!(
                    "permission scope entry is not a regular file: {}",
                    path.display()
                )));
            }
            // The authorized base directory defines the deletion scope. Doomed
            // sidecars need not contain valid JSON; reads remain fail-closed.
            std::fs::remove_file(&path).map_err(|error| {
                SessionPermissionError::io(format!("remove {}: {error}", path.display()))
            })?;
        }
        Ok(())
    })();
    // Clear every known topic and fence every registered cold read even when
    // deletion was partial; a later load must observe the remaining disk state.
    session_permission_profiles().remove_scope(&session_id, profile_id);
    if let Err(error) = &result {
        session_permission_profiles().poison(&session_id, profile_id, error.clone());
    }
    result
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

    #[test]
    fn should_use_one_identity_for_base_and_explicit_default_topic() {
        let directory = tempfile::tempdir().unwrap();
        let base = SessionKey::with_profile("unit-default", "api", "shared");
        let alias = SessionKey(format!("{}#default", base.0));
        let state = StoredSessionPermissionProfile {
            selection: PermissionProfileSelection {
                mode: PermissionProfileMode::ReadOnly,
                network: PermissionNetworkPolicy::Deny,
            },
            approval_policy: Some(ApprovalPolicy::Never),
        };
        persist_session_permission(directory.path(), Some("unit-default"), &base, state).unwrap();
        assert_eq!(
            load_session_permission(directory.path(), Some("unit-default"), &alias)
                .unwrap()
                .unwrap()
                .selection,
            state.selection
        );
        assert_eq!(
            session_permission_profiles()
                .get_state_explicit(&base, Some("unit-default"))
                .unwrap()
                .unwrap()
                .selection,
            session_permission_profiles()
                .get_state_explicit(&alias, Some("unit-default"))
                .unwrap()
                .unwrap()
                .selection
        );
        clear_session_permission(directory.path(), Some("unit-default"), &alias).unwrap();
        assert!(!permission_record_path(directory.path(), Some("unit-default"), &base).exists());
        assert!(
            session_permission_profiles()
                .get_state_explicit(&base, Some("unit-default"))
                .is_none()
        );
    }

    #[test]
    fn should_bound_long_base_and_topic_sidecars_and_reload_cold() {
        let directory = tempfile::tempdir().unwrap();
        let state = StoredSessionPermissionProfile {
            selection: PermissionProfileSelection {
                mode: PermissionProfileMode::ReadOnly,
                network: PermissionNetworkPolicy::Deny,
            },
            approval_policy: Some(ApprovalPolicy::Never),
        };
        let long_base =
            SessionKey::with_profile("unit-long", "api", &format!("web-{}", "x".repeat(260)));
        let long_topic =
            SessionKey::with_profile_topic("unit-long", "api", "bounded-topic", &"t".repeat(260));
        for session in [long_base, long_topic] {
            let path = permission_record_path(directory.path(), Some("unit-long"), &session);
            assert!(
                path.components()
                    .all(|component| component.as_os_str().len() <= 255),
                "unbounded sidecar path: {}",
                path.display()
            );
            persist_session_permission(directory.path(), Some("unit-long"), &session, state)
                .unwrap();
            session_permission_profiles().remove(&session, Some("unit-long"));
            assert_eq!(
                load_session_permission(directory.path(), Some("unit-long"), &session)
                    .unwrap()
                    .unwrap()
                    .selection,
                state.selection
            );
        }
        let medium =
            SessionKey::with_profile_topic("unit-long", "api", &"m".repeat(220), &"n".repeat(220));
        let legacy = directory
            .path()
            .join("session-permissions")
            .join("unit-long")
            .join(octos_bus::session::encode_path_component(medium.base_key()))
            .join(format!(
                "{}.json",
                octos_bus::session::encode_path_component(medium.topic().unwrap())
            ));
        assert_eq!(
            permission_record_path(directory.path(), Some("unit-long"), &medium),
            legacy,
            "valid legacy paths must not be relocated by the bounded encoding"
        );
    }

    #[test]
    fn should_not_publish_stale_cold_read_after_set_or_delete() {
        let store = SessionPermissionProfileStore::default();
        let session = SessionKey::with_profile("unit-race", "api", "cold-read");
        let key = SessionPermissionProfileStore::key(&session, Some("unit-race"));
        let old = StoredSessionPermissionProfile::default();
        let restricted = StoredSessionPermissionProfile {
            selection: PermissionProfileSelection {
                mode: PermissionProfileMode::ReadOnly,
                network: PermissionNetworkPolicy::Deny,
            },
            approval_policy: Some(ApprovalPolicy::Never),
        };

        let before_set = store.generation(&key);
        store.set(
            session.clone(),
            Some("unit-race"),
            restricted.selection,
            restricted.approval_policy,
        );
        assert!(!store.publish_read(key.clone(), before_set, Ok(old)));
        assert_eq!(
            store
                .get_state_explicit(&session, Some("unit-race"))
                .unwrap()
                .unwrap()
                .selection,
            restricted.selection
        );

        let before_delete = store.generation(&key);
        store.remove(&session, Some("unit-race"));
        assert!(!store.publish_read(key, before_delete, Ok(old)));
        assert!(
            store
                .get_state_explicit(&session, Some("unit-race"))
                .is_none()
        );

        let topic = SessionKey(format!("{}#work", session.0));
        let (topic_key, before_scope_delete) = store.begin_read(&topic, Some("unit-race"));
        store.remove_scope(&session, Some("unit-race"));
        assert!(!store.publish_read(topic_key, before_scope_delete, Ok(old)));
    }
}
