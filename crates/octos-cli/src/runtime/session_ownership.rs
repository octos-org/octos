//! Durable ownership for authenticated raw ids that use the legacy shared
//! SessionManager instead of a per-profile runtime. Without this marker the raw
//! transcript contains no profile identity, so a globally readable path is not
//! proof that the current user owns it.

use std::{
    io::Write,
    path::{Path, PathBuf},
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
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            if let Err(error) = file.write_all(&body).and_then(|_| file.sync_all()) {
                drop(file);
                let _ = std::fs::remove_file(&path);
                return Err(RawSessionOwnershipError::Invalid(error.to_string()));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            read_marker(data_dir, profile_id, session_id)
        }
        Err(error) => Err(RawSessionOwnershipError::Invalid(error.to_string())),
    }
}
