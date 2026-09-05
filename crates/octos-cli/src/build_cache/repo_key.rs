//! Stable per-repository pool key (design §1.2).
//!
//! `repo_key = hex(sha256(canonicalized repo path))[0..12]` — the first 12
//! hex characters (48 bits) of the SHA-256 of the canonicalized absolute
//! path of the MAIN repository (the `workspace_root` handed to peer
//! staging), not the peer's own clone. All peers derived from one
//! repository therefore share one pool.
//!
//! Canonicalization mirrors why sandbox/mod.rs canonicalizes HOME and
//! CARGO_HOME: symlinks (e.g. `/tmp` → `/private/tmp` on macOS) must not
//! mint two keys — and two pools — for one repository. Collision risk at 48
//! bits is birthday-bounded around tens of millions of repositories; even
//! on collision the consequence is two repositories sharing a pool (cache
//! churn), never a broken I1 — the lock still serializes writers.

use std::fmt;
use std::path::Path;

use sha2::{Digest, Sha256};

/// Number of hex characters kept from the sha256 digest.
const KEY_LEN: usize = 12;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoKeyParseError;

impl fmt::Display for RepoKeyParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "repo key must be {KEY_LEN} lowercase hex characters (sha256 prefix)"
        )
    }
}

impl std::error::Error for RepoKeyParseError {}

/// A validated pool key: 12 lowercase hex characters (sha256 prefix).
///
/// Constructed via [`repo_key_for_path`]; parsed back from a directory name
/// via [`RepoKey::parse`] when scanning the pool root.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoKey(String);

impl RepoKey {
    /// Validate a directory-name-shaped key. Rejects anything that is not
    /// exactly 12 lowercase hex characters so a pool-root scan can never
    /// descend into an unrelated directory.
    pub fn parse(value: &str) -> Result<Self, RepoKeyParseError> {
        let valid = value.len() == KEY_LEN
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if valid {
            Ok(Self(value.to_owned()))
        } else {
            Err(RepoKeyParseError)
        }
    }

    /// The key string (12 lowercase hex characters).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RepoKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Compute the pool key for a repository path (§1.2).
///
/// The path is canonicalized (`std::fs::canonicalize`, symlink-resolving);
/// the canonical UTF-8 string with any trailing `/` removed is hashed.
/// Returns `None` when the path does not exist or is not valid UTF-8 —
/// the pool key must be stable across sessions, and best-effort
/// non-canonical fallbacks would silently split one repository into two
/// pools the moment a symlink is involved.
pub fn repo_key_for_path(repo: &Path) -> Option<RepoKey> {
    let canonical = std::fs::canonicalize(repo).ok()?;
    let mut input = canonical.into_os_string();
    // A canonical path from `canonicalize` has no trailing separator on
    // Unix; strip defensively so the hashed string is platform- and
    // spelling-independent.
    if let Some(s) = input.to_str() {
        let trimmed = s.trim_end_matches('/');
        input = std::ffi::OsString::from(trimmed);
    }
    let bytes = input.as_encoded_bytes().to_vec();
    let digest = Sha256::digest(&bytes);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    Some(RepoKey(hex[..KEY_LEN].to_owned()))
}

/// Back-compat convenience: the key as a bare `String` for callers that
/// only need the pool directory name.
pub fn repo_key(repo: &Path) -> Option<String> {
    repo_key_for_path(repo).map(|k| k.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_key_is_twelve_hex_chars() {
        let tmp = tempfile::tempdir().unwrap();
        let key = repo_key_for_path(tmp.path()).unwrap();
        assert_eq!(key.as_str().len(), 12);
        RepoKey::parse(key.as_str()).unwrap();
    }

    #[test]
    fn repo_key_is_stable_across_symlink_spellings() {
        // Canonicalization must fold a symlinked spelling onto one key: two
        // spellings of one repo must not mint two pools (§1.2).
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let link = tmp.path().join("link-to-repo");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&repo, &link).unwrap();
        #[cfg(not(unix))]
        let link = repo.clone(); // no symlink spellings to fold elsewhere
        assert_eq!(
            repo_key_for_path(&repo).unwrap(),
            repo_key_for_path(&link).unwrap()
        );
    }

    #[test]
    fn repo_key_differs_for_different_repos() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("alpha");
        let b = tmp.path().join("beta");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        assert_ne!(
            repo_key_for_path(&a).unwrap(),
            repo_key_for_path(&b).unwrap()
        );
    }

    #[test]
    fn repo_key_missing_path_is_none() {
        assert!(repo_key_for_path(Path::new("/nonexistent/octos/nope")).is_none());
    }

    #[test]
    fn parse_rejects_non_hex_and_wrong_length() {
        assert!(RepoKey::parse("not-a-key!").is_err());
        assert!(RepoKey::parse("ABCDEF012345").is_err()); // uppercase rejected
        assert!(RepoKey::parse("abc").is_err());
        assert!(RepoKey::parse("abc123abc1234").is_err());
        assert!(RepoKey::parse("").is_err());
        assert!(RepoKey::parse("deadbeefcafe").is_ok());
    }
}
