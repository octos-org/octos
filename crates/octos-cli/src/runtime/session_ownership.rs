//! Durable ownership for authenticated raw ids that use the legacy shared
//! SessionManager instead of a per-profile runtime. Without this marker the raw
//! transcript contains no profile identity, so a globally readable path is not
//! proof that the current user owns it.

use std::{
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use octos_core::SessionKey;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawSessionOwnershipError {
    Missing,
    WrongOwner(String),
    Invalid(String),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnerRecord {
    version: u32,
    profile_id: String,
    session_id: String,
}

/// The legacy flat store treats an omitted topic and `#default` as one record.
/// Every ownership path and payload uses that same canonical identity.
pub fn raw_storage_identity(session_id: &SessionKey) -> SessionKey {
    if session_id.topic() == Some("default") {
        SessionKey(session_id.base_key().to_owned())
    } else {
        session_id.clone()
    }
}

fn marker_path(data_dir: &Path, session_id: &SessionKey) -> PathBuf {
    let filename = octos_bus::SessionManager::bounded_session_file_name(session_id);
    let stem = filename.strip_suffix(".jsonl").unwrap_or(&filename);
    data_dir.join("session-owners").join(format!("{stem}.json"))
}

fn read_marker(
    data_dir: &Path,
    profile_id: &str,
    session_id: &SessionKey,
) -> Result<(), RawSessionOwnershipError> {
    let storage_id = raw_storage_identity(session_id);
    let path = marker_path(data_dir, &storage_id);
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(RawSessionOwnershipError::Missing);
        }
        Err(error) => return Err(RawSessionOwnershipError::Invalid(error.to_string())),
    };
    let record: OwnerRecord = serde_json::from_str(&body)
        .map_err(|error| RawSessionOwnershipError::Invalid(error.to_string()))?;
    if record.version != 1 || record.session_id != storage_id.0 {
        return Err(RawSessionOwnershipError::Invalid(
            "raw session owner record is invalid".into(),
        ));
    }
    if record.profile_id != profile_id {
        return Err(RawSessionOwnershipError::WrongOwner(record.profile_id));
    }
    Ok(())
}

pub fn authorize_raw_session(
    data_dir: &Path,
    profile_id: &str,
    session_id: &SessionKey,
) -> Result<(), RawSessionOwnershipError> {
    read_marker(data_dir, profile_id, session_id)
}

/// Atomically claim a genuinely new raw storage identity, or verify the
/// existing owner. An existing unowned transcript can never be claimed merely
/// by knowing its raw id after an upgrade or restart.
pub fn claim_raw_session(
    data_dir: &Path,
    profile_id: &str,
    session_id: &SessionKey,
    storage_exists: bool,
) -> Result<(), RawSessionOwnershipError> {
    if storage_exists {
        return read_marker(data_dir, profile_id, session_id);
    }
    let storage_id = raw_storage_identity(session_id);
    let path = marker_path(data_dir, &storage_id);
    let parent = path
        .parent()
        .ok_or_else(|| RawSessionOwnershipError::Invalid("owner marker has no parent".into()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| RawSessionOwnershipError::Invalid(error.to_string()))?;
    let record = OwnerRecord {
        version: 1,
        profile_id: profile_id.to_owned(),
        session_id: storage_id.0.clone(),
    };
    let body = serde_json::to_vec(&record)
        .map_err(|error| RawSessionOwnershipError::Invalid(error.to_string()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| RawSessionOwnershipError::Invalid("owner marker name is invalid".into()))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| RawSessionOwnershipError::Invalid(error.to_string()))?
        .as_nanos();
    let temp = parent.join(format!(".{file_name}.tmp-{}-{nonce}", std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
    {
        Ok(mut file) => {
            if let Err(error) = file.write_all(&body).and_then(|_| file.sync_all()) {
                drop(file);
                let _ = std::fs::remove_file(&temp);
                return Err(RawSessionOwnershipError::Invalid(error.to_string()));
            }
        }
        Err(error) => return Err(RawSessionOwnershipError::Invalid(error.to_string())),
    }
    match std::fs::hard_link(&temp, &path) {
        Ok(()) => {
            let _ = std::fs::remove_file(&temp);
            Ok(())
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                read_marker(data_dir, profile_id, session_id)
            } else {
                Err(RawSessionOwnershipError::Invalid(error.to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_retry_a_claim_after_a_crash_left_only_a_stale_temp_file() {
        let tempdir = tempfile::tempdir().unwrap();
        let data_dir = tempdir.path();
        let session_id = SessionKey("raw-session".into());
        let storage_id = raw_storage_identity(&session_id);
        let marker = marker_path(data_dir, &storage_id);
        let parent = marker.parent().unwrap();
        std::fs::create_dir_all(parent).unwrap();
        let stale_temp = parent.join(format!(
            ".{}.tmp-999999-stale",
            marker.file_name().unwrap().to_str().unwrap()
        ));
        std::fs::write(&stale_temp, "partial owner record").unwrap();

        claim_raw_session(data_dir, "profile-a", &session_id, false).unwrap();

        assert_eq!(
            authorize_raw_session(data_dir, "profile-a", &session_id),
            Ok(())
        );
        assert_eq!(
            claim_raw_session(data_dir, "profile-b", &session_id, false),
            Err(RawSessionOwnershipError::WrongOwner("profile-a".to_owned()))
        );
        assert!(stale_temp.is_file(), "the stale temp file is left inert");
    }
}
