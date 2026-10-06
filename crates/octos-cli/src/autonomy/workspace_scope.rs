//! Workspace identity without changing the path used to run a worker.
//!
//! Ordinary UTF-8 keys keep their historical spelling/hash. Non-UTF-8 keys
//! use a NUL-prefixed escape (a real filesystem path cannot contain NUL).
//! New peer task stamps are tagged for ALL roots: a literal relative path
//! named `2f746d70` must not be mistaken for the old untagged hex of `/tmp`.

use std::path::Path;

const BYTES_PREFIX: &str = "\0octos-workspace-bytes-v1:";
const STAMP_PREFIX: &str = "\0octos-workspace-stamp-v1:";

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct WorkspaceScope(String);

impl WorkspaceScope {
    pub(crate) fn from_path(path: &Path) -> Option<Self> {
        Self::from_bytes(path.as_os_str().as_encoded_bytes())
    }

    pub(crate) fn from_raw(path: Option<&str>) -> Option<Self> {
        path.and_then(|path| Self::from_bytes(path.as_bytes()))
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.is_empty() {
            return None;
        }
        Some(Self(match std::str::from_utf8(bytes) {
            Ok(path) if !path.contains('\0') => path.to_owned(),
            _ => format!("{BYTES_PREFIX}{}", hex(bytes)),
        }))
    }

    pub(crate) fn from_key(key: &str) -> std::io::Result<Option<Self>> {
        if let Some(body) = key.strip_prefix(BYTES_PREFIX) {
            let scope = Self::from_bytes(&unhex(body)?);
            if scope.as_ref().map(Self::key) != Some(key) {
                return Err(invalid_scope());
            }
            Ok(scope)
        } else if key.contains('\0') {
            Err(invalid_scope())
        } else {
            Ok(Self::from_raw(Some(key)))
        }
    }

    /// New tagged wire or an already canonical/raw purge argument. Never
    /// interpret untagged hex here: native callers own real UTF-8 paths.
    pub(crate) fn from_argument(value: &str) -> std::io::Result<Option<Self>> {
        if let Some(body) = value.strip_prefix(STAMP_PREFIX) {
            let bytes = unhex(body)?;
            if bytes.is_empty() {
                return Err(invalid_scope());
            }
            Ok(Self::from_bytes(&bytes))
        } else {
            Self::from_key(value)
        }
    }

    /// Only call with proven peer-stamp provenance. Legacy #13 roots were
    /// raw, #21 roots untagged hex. Their production absolute-root spelling
    /// distinguishes them; old ambiguous relative peer roots have no format
    /// discriminator. New tagged stamps do not inherit that ambiguity.
    pub(crate) fn from_peer_stamp(value: &str) -> std::io::Result<Option<Self>> {
        if value.contains('\0') {
            return Self::from_argument(value);
        }
        if let Ok(bytes) = unhex(value)
            && (bytes.starts_with(b"/")
                || bytes.starts_with(b"\\\\")
                || (bytes.len() >= 3
                    && bytes[0].is_ascii_alphabetic()
                    && bytes[1] == b':'
                    && matches!(bytes[2], b'/' | b'\\')))
        {
            return Ok(Self::from_bytes(&bytes));
        }
        Ok(Self::from_raw(Some(value)))
    }

    pub(crate) fn key(&self) -> &str {
        &self.0
    }

    fn bytes(&self) -> Vec<u8> {
        match self.0.strip_prefix(BYTES_PREFIX) {
            Some(body) => unhex(body).expect("scope constructors validate the escape"),
            None => self.0.as_bytes().to_vec(),
        }
    }

    pub(crate) fn display_path(&self) -> String {
        String::from_utf8_lossy(&self.bytes()).into_owned()
    }

    pub(crate) fn legacy_hex(&self) -> String {
        hex(&self.bytes())
    }

    pub(crate) fn peer_stamp(path: &Path) -> Option<String> {
        Self::from_path(path).map(|scope| format!("{STAMP_PREFIX}{}", scope.legacy_hex()))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(value: &str) -> std::io::Result<Vec<u8>> {
    if value.is_empty() || value.len() % 2 != 0 {
        return Err(invalid_scope());
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16).ok_or_else(invalid_scope)?;
            let low = (pair[1] as char).to_digit(16).ok_or_else(invalid_scope)?;
            Ok((high * 16 + low) as u8)
        })
        .collect()
}

fn invalid_scope() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "invalid versioned workspace scope",
    )
}
