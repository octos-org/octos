//! Tenant-owned copies of files an agent delivers to a session.
//!
//! A delivered file (`send_file`, a background tool result) can live anywhere
//! the agent could write — including an approved external project folder —
//! and the transcript records that original path, which is what a local
//! client (the TUI) should show. `/api/files` deliberately serves only paths
//! under the tenant's data dir, so a browser could never download such a
//! file. At delivery time a copy is therefore stored at a location keyed by
//! the exact delivered path ([`delivered_copy_path`]); `/api/files` serves it
//! for a `(session, path)` request. The copy existing is the proof that the
//! agent delivered exactly that path in that session. Profile ownership is the
//! authorization boundary: the session and path select a copy within that
//! profile, not a separate per-session authorization grant (#2543-①).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use octos_core::SessionKey;
use serde::{Deserialize, Serialize};
use tracing::warn;

pub const DEFAULT_DELIVERED_MAX_BYTES: u64 = 50 * 1024 * 1024 * 1024;

/// Per-profile retention bounds for external delivered copies. Entry count is
/// unlimited unless explicitly configured; no TTL or implicit count default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ArtifactRetentionPolicy {
    #[serde(default = "default_delivered_max_bytes")]
    pub max_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_entries: Option<u64>,
}

const fn default_delivered_max_bytes() -> u64 {
    DEFAULT_DELIVERED_MAX_BYTES
}

impl Default for ArtifactRetentionPolicy {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_DELIVERED_MAX_BYTES,
            max_entries: None,
        }
    }
}

/// `<data_dir>/users/<encoded base key>/workspace` — a session's tenant-owned
/// workspace in the canonical multi-tenant layout.
pub fn session_workspace_dir(data_dir: &Path, key: &SessionKey) -> PathBuf {
    let encoded = crate::session::encode_path_component(key.base_key());
    data_dir.join("users").join(encoded).join("workspace")
}

/// Where delivered files are copied for download.
pub fn session_artifact_dir(data_dir: &Path, key: &SessionKey) -> PathBuf {
    session_workspace_dir(data_dir, key).join(".artifacts")
}

/// Where the tenant-owned copy of a file delivered in `key` as `reference`
/// (the exact media string the transcript records) is stored:
/// `<artifact dir>/delivered/<sha256(reference)>/<file name>`. Deterministic,
/// so `/api/files` can find it from the request alone.
pub fn delivered_copy_path(data_dir: &Path, key: &SessionKey, reference: &str) -> PathBuf {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(reference.as_bytes());
    let bucket: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    session_artifact_dir(data_dir, key)
        .join("delivered")
        .join(bucket)
        .join(sanitize_artifact_name(Path::new(reference)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum DeliveredState {
    Present,
    Evicted,
}

#[derive(Debug, Serialize, Deserialize)]
struct DeliveredEntry {
    state: DeliveredState,
    size_bytes: u64,
    last_used_seq: u64,
    /// Detect a replacement committed before its manifest update was saved.
    #[serde(default)]
    modified_unix_ms: u128,
    /// Durable write intent. Recovery must refresh recency even when a
    /// replacement has the same size and coarse-mtime fingerprint.
    #[serde(default)]
    write_in_progress: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct DeliveredManifest {
    schema_version: u32,
    next_seq: u64,
    entries: BTreeMap<String, DeliveredEntry>,
}

impl Default for DeliveredManifest {
    fn default() -> Self {
        Self {
            schema_version: 1,
            next_seq: 1,
            entries: BTreeMap::new(),
        }
    }
}

/// An already-open delivered download. The profile lock is released before
/// the caller reads `file`, so unlink/replacement races cannot mix bytes.
pub enum DeliveredDownloadOpen {
    Present { path: PathBuf, file: std::fs::File },
    Missing,
    Evicted,
}

#[cfg(test)]
thread_local! {
    static CRASH_AFTER_RENAME_FOR_TEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn manifest_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(".delivered-artifacts")
}

fn manifest_path(data_dir: &Path) -> PathBuf {
    manifest_dir(data_dir).join("manifest.json")
}

fn lock_path(data_dir: &Path) -> PathBuf {
    manifest_dir(data_dir).join("retention.lock")
}

fn initialized_path(data_dir: &Path) -> PathBuf {
    manifest_dir(data_dir).join("initialized")
}

fn lock_profile(data_dir: &Path) -> std::io::Result<std::fs::File> {
    std::fs::create_dir_all(manifest_dir(data_dir))?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path(data_dir))?;
    file.lock_exclusive()?;
    Ok(file)
}

fn load_manifest(data_dir: &Path) -> std::io::Result<DeliveredManifest> {
    let path = manifest_path(data_dir);
    if !path.exists() {
        if initialized_path(data_dir).exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "initialized delivered-artifact manifest is missing; refusing legacy re-adoption",
            ));
        }
        return Ok(DeliveredManifest::default());
    }
    let bytes = std::fs::read(path)?;
    let manifest: DeliveredManifest = serde_json::from_slice(&bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if manifest.schema_version != 1 || manifest.next_seq == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unsupported delivered-artifact manifest",
        ));
    }
    Ok(manifest)
}

fn save_manifest(data_dir: &Path, manifest: &DeliveredManifest) -> std::io::Result<()> {
    std::fs::create_dir_all(manifest_dir(data_dir))?;
    let destination = manifest_path(data_dir);
    let temporary = manifest_dir(data_dir).join(format!(".partial-{}", uuid::Uuid::now_v7()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let bytes = serde_json::to_vec(manifest)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let result = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&temporary, &destination)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result?;
    // Once initialized, a missing manifest is corruption/recovery input, not
    // permission to resurrect tombstoned files as a legacy installation.
    let marker = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(initialized_path(data_dir))?;
    marker.sync_all()
}

fn manifest_key(data_dir: &Path, path: &Path) -> std::io::Result<String> {
    let relative = path.strip_prefix(data_dir).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "artifact key escapes profile root",
        )
    })?;
    Ok(relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/"))
}

fn delivered_key_parts(key: &str) -> std::io::Result<(Vec<String>, String)> {
    let parts: Vec<&str> = key.split('/').collect();
    let valid = parts.len() == 7
        && parts[0] == "users"
        && !parts[1].is_empty()
        && parts[2] == "workspace"
        && parts[3] == ".artifacts"
        && parts[4] == "delivered"
        && parts[5].len() == 32
        && parts[5]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        && !parts[6].is_empty()
        && parts[6] != "."
        && parts[6] != ".."
        && parts.iter().all(|part| !part.contains(['\\', '\0']));
    if !valid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "manifest key is not a confined delivered-artifact path",
        ));
    }
    let leaf = parts[6].to_string();
    Ok((
        parts[..6].iter().map(|part| (*part).to_string()).collect(),
        leaf,
    ))
}

fn artifact_open_options() -> cap_primitives::fs::OpenOptions {
    use cap_primitives::fs::FollowSymlinks;
    let mut options = cap_primitives::fs::OpenOptions::new();
    options.read(true);
    options._cap_fs_ext_follow(FollowSymlinks::No);
    options
}

fn open_artifact_root(data_dir: &Path) -> std::io::Result<std::fs::File> {
    let directory =
        cap_primitives::fs::open_ambient_dir(data_dir, cap_primitives::ambient_authority())?;
    if !directory.metadata()?.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            "profile root is not a directory",
        ));
    }
    Ok(directory)
}

fn open_artifact_parent(
    data_dir: &Path,
    key: &str,
    create: bool,
) -> std::io::Result<(std::fs::File, String)> {
    let (parents, leaf) = delivered_key_parts(key)?;
    let mut directory = open_artifact_root(data_dir)?;
    for component in parents {
        directory = open_child_directory(&directory, &component, create)?;
    }
    Ok((directory, leaf))
}

fn open_child_directory(
    directory: &std::fs::File,
    component: &str,
    create: bool,
) -> std::io::Result<std::fs::File> {
    let path = Path::new(component);
    if create {
        match cap_primitives::fs::create_dir(
            directory,
            path,
            &cap_primitives::fs::DirOptions::new(),
        ) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let next = cap_primitives::fs::open_dir_nofollow(directory, path)?;
    let metadata = next.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "delivered-artifact parent is a reparse point",
            ));
        }
    }
    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            "delivered-artifact parent is not a directory",
        ));
    }
    Ok(next)
}

fn open_manifest_directory(data_dir: &Path, create: bool) -> std::io::Result<std::fs::File> {
    let root = open_artifact_root(data_dir)?;
    open_child_directory(&root, ".delivered-artifacts", create)
}

fn open_pending_directory(data_dir: &Path, create: bool) -> std::io::Result<std::fs::File> {
    let manifest = open_manifest_directory(data_dir, create)?;
    open_child_directory(&manifest, "pending", create)
}

fn unix_ms(time: Option<std::time::SystemTime>) -> u128 {
    time.and_then(|time| time.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn delivered_fingerprint(data_dir: &Path, key: &str) -> std::io::Result<(u64, u128)> {
    let (parent, leaf) = open_artifact_parent(data_dir, key, false)?;
    let metadata = cap_primitives::fs::stat(
        &parent,
        Path::new(&leaf),
        cap_primitives::fs::FollowSymlinks::No,
    )?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "delivered artifact is not a regular file",
        ));
    }
    Ok((
        metadata.len(),
        unix_ms(metadata.modified().ok().map(|time| time.into_std())),
    ))
}

fn open_delivered_file(data_dir: &Path, key: &str) -> std::io::Result<std::fs::File> {
    let (parent, leaf) = open_artifact_parent(data_dir, key, false)?;
    let file = cap_primitives::fs::open(&parent, Path::new(&leaf), &artifact_open_options())?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "delivered artifact is not a regular file",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "delivered artifact is a reparse point",
            ));
        }
    }
    Ok(file)
}

fn remove_delivered_file(data_dir: &Path, key: &str) -> std::io::Result<()> {
    let (parent, leaf) = open_artifact_parent(data_dir, key, false)?;
    cap_primitives::fs::remove_file(&parent, Path::new(&leaf))
}

fn copy_delivered_replacing(data_dir: &Path, key: &str, source: &Path) -> std::io::Result<()> {
    let (parent, leaf) = open_artifact_parent(data_dir, key, true)?;
    let pending = open_pending_directory(data_dir, true)?;
    let temporary = uuid::Uuid::now_v7().to_string();
    let mut options = artifact_open_options();
    options.write(true).create_new(true);
    let mut destination = cap_primitives::fs::open(&pending, Path::new(&temporary), &options)?;
    let result = (|| {
        let mut source = std::fs::File::open(source)?;
        std::io::copy(&mut source, &mut destination)?;
        destination.sync_all()?;
        cap_primitives::fs::rename(&pending, Path::new(&temporary), &parent, Path::new(&leaf))
    })();
    if result.is_err() {
        let _ = cap_primitives::fs::remove_file(&pending, Path::new(&temporary));
    }
    result
}

fn cleanup_stale_temporary_files(data_dir: &Path) -> std::io::Result<()> {
    // All writers hold the profile lock, so anything left in these dedicated
    // temporary locations belongs to an interrupted writer, not a live one.
    let manifest = open_manifest_directory(data_dir, false)?;
    for entry in cap_primitives::fs::read_base_dir(&manifest)? {
        let entry = entry?;
        let is_manifest_partial = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_prefix(".partial-"))
            .is_some_and(|suffix| uuid::Uuid::parse_str(suffix).is_ok());
        if is_manifest_partial {
            match entry.remove_file() {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }

    let pending = match open_pending_directory(data_dir, false) {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in cap_primitives::fs::read_base_dir(&pending)? {
        let entry = entry?;
        match entry.remove_file() {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn next_seq(manifest: &mut DeliveredManifest) -> std::io::Result<u64> {
    let seq = manifest.next_seq;
    manifest.next_seq = manifest.next_seq.checked_add(1).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "LRU sequence exhausted")
    })?;
    Ok(seq)
}

fn is_delivered_file(data_dir: &Path, path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return false;
    }
    manifest_key(data_dir, path)
        .and_then(|key| delivered_key_parts(&key))
        .is_ok()
}

fn scan_legacy(
    data_dir: &Path,
    directory: &Path,
    found: &mut Vec<(std::time::SystemTime, String)>,
) -> std::io::Result<()> {
    let metadata = match std::fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Ok(());
    }
    let Ok(relative) = directory.strip_prefix(data_dir) else {
        return Ok(());
    };
    let components: Vec<_> = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect();
    let allowed = match components.len() {
        1 | 2 => components[0] == "users",
        3 => components[0] == "users" && components[2] == "workspace",
        4 => {
            components[0] == "users"
                && components[2] == "workspace"
                && components[3] == ".artifacts"
        }
        5 | 6 => {
            components[0] == "users"
                && components[2] == "workspace"
                && components[3] == ".artifacts"
                && components[4] == "delivered"
        }
        _ => false,
    };
    if !allowed {
        return Ok(());
    }
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            scan_legacy(data_dir, &path, found)?;
        } else if is_delivered_file(data_dir, &path) {
            found.push((
                metadata
                    .modified()
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH),
                manifest_key(data_dir, &path)?,
            ));
        }
    }
    Ok(())
}

fn reconcile_manifest(data_dir: &Path, manifest: &mut DeliveredManifest) -> std::io::Result<bool> {
    let mut changed = false;
    let keys: Vec<String> = manifest.entries.keys().cloned().collect();
    for key in keys {
        let fingerprint = match delivered_fingerprint(data_dir, &key) {
            Ok(fingerprint) => Some(fingerprint),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let state = manifest.entries[&key].state;
        match (state, fingerprint) {
            (DeliveredState::Present, None) => {
                let entry = manifest.entries.get_mut(&key).expect("entry exists");
                entry.state = DeliveredState::Evicted;
                entry.write_in_progress = false;
                changed = true;
            }
            (DeliveredState::Present, Some((size_bytes, modified_unix_ms))) => {
                let entry = &manifest.entries[&key];
                if entry.write_in_progress
                    || entry.size_bytes != size_bytes
                    || entry.modified_unix_ms != modified_unix_ms
                {
                    // A crash may have committed the rename but not its manifest
                    // update. Refresh both quota accounting and LRU recency.
                    let last_used_seq = next_seq(manifest)?;
                    let entry = manifest.entries.get_mut(&key).expect("entry exists");
                    entry.size_bytes = size_bytes;
                    entry.modified_unix_ms = modified_unix_ms;
                    entry.last_used_seq = last_used_seq;
                    entry.write_in_progress = false;
                    changed = true;
                }
            }
            (DeliveredState::Evicted, Some(_)) => match remove_delivered_file(data_dir, &key) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            },
            (DeliveredState::Evicted, None) => {}
        }
    }

    // Legacy adoption is a one-time upgrade step. Once the manifest exists,
    // avoid recursively scanning the whole profile on every download.
    if !manifest_path(data_dir).exists() {
        let mut legacy = Vec::new();
        scan_legacy(data_dir, &data_dir.join("users"), &mut legacy)?;
        legacy.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        for (_, key) in legacy {
            if manifest.entries.contains_key(&key) {
                continue;
            }
            let (size_bytes, modified_unix_ms) = match delivered_fingerprint(data_dir, &key) {
                Ok(fingerprint) => fingerprint,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let last_used_seq = next_seq(manifest)?;
            manifest.entries.insert(
                key,
                DeliveredEntry {
                    state: DeliveredState::Present,
                    size_bytes,
                    last_used_seq,
                    modified_unix_ms,
                    write_in_progress: false,
                },
            );
            changed = true;
        }
    }
    Ok(changed)
}

fn enforce_limits(
    data_dir: &Path,
    manifest: &mut DeliveredManifest,
    policy: ArtifactRetentionPolicy,
) -> std::io::Result<()> {
    let mut live: Vec<(u64, String, u64)> = manifest
        .entries
        .iter()
        .filter(|(_, entry)| entry.state == DeliveredState::Present)
        .map(|(key, entry)| (entry.last_used_seq, key.clone(), entry.size_bytes))
        .collect();
    live.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut total_bytes: u64 = live.iter().map(|(_, _, bytes)| *bytes).sum();
    let mut live_count = live.len() as u64;
    let mut index = 0;
    while total_bytes > policy.max_bytes
        || policy
            .max_entries
            .is_some_and(|maximum| live_count > maximum)
    {
        let Some((_, key, size_bytes)) = live.get(index).cloned() else {
            break;
        };
        // Validate the confined key before persisting the tombstone.
        delivered_key_parts(&key)?;
        let entry = manifest.entries.get_mut(&key).expect("live entry exists");
        entry.state = DeliveredState::Evicted;
        // Persist the cleaned state BEFORE unlinking. A crash can never turn
        // the entry back into an ordinary 404 or expose stale bytes as present.
        save_manifest(data_dir, manifest)?;
        match remove_delivered_file(data_dir, &key) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        total_bytes = total_bytes.saturating_sub(size_bytes);
        live_count = live_count.saturating_sub(1);
        index += 1;
    }
    Ok(())
}

fn with_manifest<T>(
    data_dir: &Path,
    policy: ArtifactRetentionPolicy,
    operation: impl FnOnce(&mut DeliveredManifest, &Path) -> std::io::Result<T>,
) -> std::io::Result<T> {
    let _lock = lock_profile(data_dir)?;
    let mut manifest = load_manifest(data_dir)?;
    cleanup_stale_temporary_files(data_dir)?;
    if reconcile_manifest(data_dir, &mut manifest)? {
        save_manifest(data_dir, &manifest)?;
    }
    enforce_limits(data_dir, &mut manifest, policy)?;
    operation(&mut manifest, data_dir)
}

/// Open and touch a delivered copy while holding the same profile lock used by
/// eviction. Only the returned descriptor is used after the lock is released.
pub fn open_delivered_download(
    data_dir: &Path,
    key: &SessionKey,
    reference: &str,
    policy: ArtifactRetentionPolicy,
) -> std::io::Result<DeliveredDownloadOpen> {
    let path = delivered_copy_path(data_dir, key, reference);
    let key_string = manifest_key(data_dir, &path)?;
    with_manifest(data_dir, policy, |manifest, root| {
        let Some(entry) = manifest.entries.get(&key_string) else {
            return Ok(DeliveredDownloadOpen::Missing);
        };
        if entry.state == DeliveredState::Evicted {
            return Ok(DeliveredDownloadOpen::Evicted);
        }
        let file = match open_delivered_file(root, &key_string) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some(entry) = manifest.entries.get_mut(&key_string) {
                    entry.state = DeliveredState::Evicted;
                    save_manifest(root, manifest)?;
                }
                return Ok(DeliveredDownloadOpen::Evicted);
            }
            Err(error) => return Err(error),
        };
        let last_used_seq = next_seq(manifest)?;
        if let Some(entry) = manifest.entries.get_mut(&key_string) {
            entry.last_used_seq = last_used_seq;
        }
        save_manifest(root, manifest)?;
        Ok(DeliveredDownloadOpen::Present { path, file })
    })
}

/// Store a downloadable copy of each delivered file that the tenant cannot
/// already download (anything outside `data_dir`). A re-delivery of the same
/// path replaces the copy, so the download is the latest delivered bytes. A
/// file that cannot be read or copied is logged and skipped: the delivery is
/// still recorded, it just has no browser download.
pub fn store_delivered_copies(data_dir: &Path, key: &SessionKey, media: &[String]) {
    store_delivered_copies_with_policy(data_dir, key, media, ArtifactRetentionPolicy::default());
}

pub fn store_delivered_copies_with_policy(
    data_dir: &Path,
    key: &SessionKey,
    media: &[String],
    policy: ArtifactRetentionPolicy,
) {
    let tenant_root = std::fs::canonicalize(data_dir).ok();
    for reference in media {
        let Ok(source) = std::fs::canonicalize(reference) else {
            warn!(path = %reference, "delivered file is not readable; no download copy");
            continue;
        };
        if tenant_root
            .as_ref()
            .is_some_and(|root| source.starts_with(root))
            || !source.is_file()
        {
            continue;
        }
        let dest = delivered_copy_path(data_dir, key, reference);
        let result = with_manifest(data_dir, policy, |manifest, root| {
            let key_string = manifest_key(root, &dest)?;
            // Persist write intent BEFORE any file commit. New keys converge
            // to Present or Evicted; replacements refresh recency even when
            // size/mtime is unchanged. An existing tombstone gets no intent,
            // so an interrupted redelivery cannot resurrect it.
            let persist_intent = match manifest.entries.get_mut(&key_string) {
                Some(entry) if entry.state == DeliveredState::Present => {
                    entry.write_in_progress = true;
                    true
                }
                Some(_) => false,
                None => {
                    let last_used_seq = next_seq(manifest)?;
                    manifest.entries.insert(
                        key_string.clone(),
                        DeliveredEntry {
                            state: DeliveredState::Present,
                            size_bytes: 0,
                            last_used_seq,
                            modified_unix_ms: 0,
                            write_in_progress: true,
                        },
                    );
                    true
                }
            };
            if persist_intent {
                save_manifest(root, manifest)?;
            }
            copy_delivered_replacing(root, &key_string, &source)?;
            #[cfg(test)]
            if CRASH_AFTER_RENAME_FOR_TEST.with(|flag| flag.get()) {
                return Err(std::io::Error::other(
                    "injected crash after delivered rename",
                ));
            }
            let (size_bytes, modified_unix_ms) = delivered_fingerprint(root, &key_string)?;
            let last_used_seq = next_seq(manifest)?;
            manifest.entries.insert(
                key_string,
                DeliveredEntry {
                    state: DeliveredState::Present,
                    size_bytes,
                    last_used_seq,
                    modified_unix_ms,
                    write_in_progress: false,
                },
            );
            save_manifest(root, manifest)?;
            enforce_limits(root, manifest, policy)
        });
        if let Err(error) = result {
            warn!(
                source = %source.display(),
                dest = %dest.display(),
                %error,
                "failed to retain the download copy of a delivered file"
            );
        }
    }
}

/// Copy each file into `artifact_dir` unless it is already inside it, reusing
/// a byte-identical earlier copy of the same name. Returns the path to use for
/// each input (the input itself when it cannot be copied).
pub fn copy_into_artifact_dir(artifact_dir: &Path, media: &[String]) -> Vec<String> {
    if let Err(error) = std::fs::create_dir_all(artifact_dir) {
        warn!(
            path = %artifact_dir.display(),
            %error,
            "failed to create session artifact directory"
        );
        return media.to_vec();
    }

    let canonical_artifact_dir =
        std::fs::canonicalize(artifact_dir).unwrap_or_else(|_| artifact_dir.to_path_buf());

    media
        .iter()
        .map(|raw| {
            let source_path = PathBuf::from(raw);
            if source_path.starts_with(&canonical_artifact_dir) {
                return raw.clone();
            }

            let canonical_source = match std::fs::canonicalize(&source_path) {
                Ok(path) => path,
                Err(error) => {
                    warn!(path = %raw, %error, "failed to canonicalize media source");
                    return raw.clone();
                }
            };

            if canonical_source.starts_with(&canonical_artifact_dir) {
                return canonical_source.to_string_lossy().to_string();
            }

            let safe_name = sanitize_artifact_name(&canonical_source);
            if let Some(existing) =
                find_matching_artifact_copy(&canonical_artifact_dir, &canonical_source, &safe_name)
            {
                return existing.to_string_lossy().to_string();
            }
            let dest = canonical_artifact_dir.join(format!("{}-{safe_name}", uuid::Uuid::now_v7()));

            if canonical_source == dest {
                return canonical_source.to_string_lossy().to_string();
            }

            match std::fs::copy(&canonical_source, &dest) {
                Ok(_) => dest.to_string_lossy().to_string(),
                Err(error) => {
                    warn!(
                        source = %canonical_source.display(),
                        dest = %dest.display(),
                        %error,
                        "failed to materialize media into session artifacts"
                    );
                    raw.clone()
                }
            }
        })
        .collect()
}

fn sanitize_artifact_name(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "artifact".to_string());
    name.replace(['/', '\\', '\0'], "_")
}

fn find_matching_artifact_copy(
    artifact_dir: &Path,
    source: &Path,
    safe_name: &str,
) -> Option<PathBuf> {
    let source_meta = std::fs::metadata(source).ok()?;
    let source_len = source_meta.len();
    let source_bytes = std::fs::read(source).ok()?;

    std::fs::read_dir(artifact_dir)
        .ok()?
        .filter_map(|entry| entry.ok().map(|item| item.path()))
        .find(|candidate| {
            if !candidate.is_file() {
                return false;
            }
            let Some(name) = candidate.file_name().and_then(|value| value.to_str()) else {
                return false;
            };
            if name != safe_name && !name.ends_with(&format!("-{safe_name}")) {
                return false;
            }
            let Ok(candidate_meta) = std::fs::metadata(candidate) else {
                return false;
            };
            if candidate_meta.len() != source_len {
                return false;
            }
            std::fs::read(candidate)
                .map(|bytes| bytes == source_bytes)
                .unwrap_or(false)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SessionKey {
        SessionKey::with_profile("dev", "api", "web-deliver")
    }

    fn external_reference(project: &Path, name: &str, bytes: &[u8]) -> String {
        let path = project.join(name);
        std::fs::write(&path, bytes).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn read_present(data_dir: &Path, reference: &str, policy: ArtifactRetentionPolicy) -> Vec<u8> {
        use std::io::Read;
        let opened = open_delivered_download(data_dir, &key(), reference, policy).unwrap();
        let DeliveredDownloadOpen::Present { mut file, .. } = opened else {
            panic!("expected present delivered download");
        };
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn should_never_delete_files_when_manifest_is_corrupt() {
        let tenant = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let a = external_reference(project.path(), "a.txt", b"a");
        let b = external_reference(project.path(), "b.txt", b"b");
        store_delivered_copies(tenant.path(), &key(), std::slice::from_ref(&a));
        let old_copy = delivered_copy_path(tenant.path(), &key(), &a);
        std::fs::write(manifest_path(tenant.path()), b"not-json").unwrap();

        store_delivered_copies(tenant.path(), &key(), std::slice::from_ref(&b));
        assert!(old_copy.is_file());
        assert!(!delivered_copy_path(tenant.path(), &key(), &b).exists());
        assert!(
            open_delivered_download(
                tenant.path(),
                &key(),
                &a,
                ArtifactRetentionPolicy::default()
            )
            .is_err()
        );
    }

    #[test]
    fn should_reject_non_delivered_manifest_keys_without_deleting() {
        let tenant = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let reference = external_reference(project.path(), "a.txt", b"a");
        store_delivered_copies(tenant.path(), &key(), std::slice::from_ref(&reference));
        let sensitive = tenant.path().join("users/x/workspace/keep.txt");
        std::fs::create_dir_all(sensitive.parent().unwrap()).unwrap();
        std::fs::write(&sensitive, b"keep").unwrap();
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(manifest_path(tenant.path())).unwrap()).unwrap();
        manifest["entries"]["users/x/workspace/keep.txt"] = serde_json::json!({
            "state": "evicted",
            "size_bytes": 4,
            "last_used_seq": 0,
            "modified_unix_ms": 0
        });
        std::fs::write(
            manifest_path(tenant.path()),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        assert!(
            open_delivered_download(
                tenant.path(),
                &key(),
                &reference,
                ArtifactRetentionPolicy::default()
            )
            .is_err()
        );
        assert_eq!(std::fs::read(sensitive).unwrap(), b"keep");
    }

    #[test]
    fn should_not_read_through_a_symlinked_delivered_ancestor() {
        #[cfg(unix)]
        {
            let tenant = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            let external = tempfile::tempdir().unwrap();
            let reference = external_reference(project.path(), "a.txt", b"a");
            store_delivered_copies(tenant.path(), &key(), std::slice::from_ref(&reference));
            let copy = delivered_copy_path(tenant.path(), &key(), &reference);
            let bucket = copy.parent().unwrap();
            let backup = tenant.path().join("bucket-backup");
            std::fs::rename(bucket, &backup).unwrap();
            std::fs::write(external.path().join("a.txt"), b"secret").unwrap();
            std::os::unix::fs::symlink(external.path(), bucket).unwrap();

            assert!(
                open_delivered_download(
                    tenant.path(),
                    &key(),
                    &reference,
                    ArtifactRetentionPolicy::default()
                )
                .is_err()
            );
            assert_eq!(
                std::fs::read(external.path().join("a.txt")).unwrap(),
                b"secret"
            );
        }
    }

    #[test]
    fn should_fail_closed_when_initialized_manifest_is_missing() {
        let tenant = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let reference = external_reference(project.path(), "a.txt", b"a");
        store_delivered_copies(tenant.path(), &key(), std::slice::from_ref(&reference));
        let copy = delivered_copy_path(tenant.path(), &key(), &reference);
        std::fs::remove_file(manifest_path(tenant.path())).unwrap();

        assert!(
            open_delivered_download(
                tenant.path(),
                &key(),
                &reference,
                ArtifactRetentionPolicy::default()
            )
            .is_err()
        );
        assert!(copy.is_file());
    }

    #[test]
    fn should_recover_new_key_after_rename_crash_and_restore_quota() {
        let tenant = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let policy = ArtifactRetentionPolicy {
            max_bytes: 4,
            max_entries: None,
        };
        let old = external_reference(project.path(), "old.txt", b"old!");
        let new = external_reference(project.path(), "new.txt", b"new!");
        store_delivered_copies_with_policy(
            tenant.path(),
            &key(),
            std::slice::from_ref(&old),
            policy,
        );

        CRASH_AFTER_RENAME_FOR_TEST.with(|flag| flag.set(true));
        store_delivered_copies_with_policy(
            tenant.path(),
            &key(),
            std::slice::from_ref(&new),
            policy,
        );
        CRASH_AFTER_RENAME_FOR_TEST.with(|flag| flag.set(false));

        // The rename committed but the new entry was not saved. Recovery must
        // adopt and account for this key instead of leaving an unbounded orphan.
        assert_eq!(
            std::fs::read(delivered_copy_path(tenant.path(), &key(), &new)).unwrap(),
            b"new!"
        );
        assert_eq!(read_present(tenant.path(), &new, policy), b"new!");
        assert!(matches!(
            open_delivered_download(tenant.path(), &key(), &old, policy).unwrap(),
            DeliveredDownloadOpen::Evicted
        ));
        let manifest: DeliveredManifest =
            serde_json::from_slice(&std::fs::read(manifest_path(tenant.path())).unwrap()).unwrap();
        let new_key = manifest_key(
            tenant.path(),
            &delivered_copy_path(tenant.path(), &key(), &new),
        )
        .unwrap();
        let entry = &manifest.entries[&new_key];
        assert_eq!(entry.state, DeliveredState::Present);
        assert_eq!(entry.size_bytes, 4);
    }

    #[test]
    fn should_not_resurrect_evicted_entry_after_interrupted_redelivery() {
        let tenant = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let reference = external_reference(project.path(), "a.txt", b"aa");
        store_delivered_copies_with_policy(
            tenant.path(),
            &key(),
            std::slice::from_ref(&reference),
            ArtifactRetentionPolicy {
                max_bytes: 0,
                max_entries: None,
            },
        );
        let copy = delivered_copy_path(tenant.path(), &key(), &reference);
        assert!(!copy.exists());

        CRASH_AFTER_RENAME_FOR_TEST.with(|flag| flag.set(true));
        store_delivered_copies(tenant.path(), &key(), std::slice::from_ref(&reference));
        CRASH_AFTER_RENAME_FOR_TEST.with(|flag| flag.set(false));
        assert!(copy.is_file());

        assert!(matches!(
            open_delivered_download(
                tenant.path(),
                &key(),
                &reference,
                ArtifactRetentionPolicy::default()
            )
            .unwrap(),
            DeliveredDownloadOpen::Evicted
        ));
        assert!(!copy.exists());
    }
}
