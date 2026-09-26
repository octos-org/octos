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
//! agent delivered exactly that path in that session.

use std::path::{Path, PathBuf};

use octos_core::SessionKey;
use tracing::warn;

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

/// Store a downloadable copy of each delivered file that the tenant cannot
/// already download (anything outside `data_dir`). A re-delivery of the same
/// path replaces the copy, so the download is the latest delivered bytes. A
/// file that cannot be read or copied is logged and skipped: the delivery is
/// still recorded, it just has no browser download.
pub fn store_delivered_copies(data_dir: &Path, key: &SessionKey, media: &[String]) {
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
        if let Err(error) = copy_replacing(&source, &dest) {
            warn!(
                source = %source.display(),
                dest = %dest.display(),
                %error,
                "failed to store the download copy of a delivered file"
            );
        }
    }
}

/// Copy through a sibling temp file and rename, so a concurrent download
/// never sees a half-written copy.
fn copy_replacing(source: &Path, dest: &Path) -> std::io::Result<()> {
    let dir = dest.parent().unwrap_or(dest);
    std::fs::create_dir_all(dir)?;
    let temp = dir.join(format!(".partial-{}", uuid::Uuid::now_v7()));
    std::fs::copy(source, &temp)?;
    std::fs::rename(&temp, dest).inspect_err(|_| {
        let _ = std::fs::remove_file(&temp);
    })
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

    #[test]
    fn should_store_a_download_copy_of_a_file_outside_the_tenant_root() {
        let tenant = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let source = project.path().join("p20-art.png");
        std::fs::write(&source, b"png bytes").unwrap();
        let reference = source.to_string_lossy().into_owned();

        store_delivered_copies(tenant.path(), &key(), std::slice::from_ref(&reference));

        let copy = delivered_copy_path(tenant.path(), &key(), &reference);
        assert!(copy.starts_with(session_artifact_dir(tenant.path(), &key())));
        assert!(copy.ends_with("p20-art.png"));
        assert_eq!(std::fs::read(&copy).unwrap(), b"png bytes");
    }

    #[test]
    fn should_not_copy_a_file_the_tenant_can_already_download() {
        let tenant = tempfile::tempdir().unwrap();
        let inside = tenant.path().join("users/x/workspace/report.pdf");
        std::fs::create_dir_all(inside.parent().unwrap()).unwrap();
        std::fs::write(&inside, b"pdf").unwrap();
        let reference = inside.to_string_lossy().into_owned();

        store_delivered_copies(tenant.path(), &key(), std::slice::from_ref(&reference));

        assert!(!session_artifact_dir(tenant.path(), &key()).exists());
    }

    #[test]
    fn should_replace_the_copy_when_the_same_path_is_delivered_again() {
        let tenant = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let source = project.path().join("deck.pptx");
        let reference = source.to_string_lossy().into_owned();
        std::fs::write(&source, b"v1").unwrap();
        store_delivered_copies(tenant.path(), &key(), std::slice::from_ref(&reference));
        std::fs::write(&source, b"v2").unwrap();
        store_delivered_copies(tenant.path(), &key(), std::slice::from_ref(&reference));

        let copy = delivered_copy_path(tenant.path(), &key(), &reference);
        assert_eq!(std::fs::read(&copy).unwrap(), b"v2");
        assert_eq!(
            std::fs::read_dir(copy.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn should_keep_same_named_files_from_different_folders_apart() {
        let tenant = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let a = project.path().join("a/chart.png");
        let b = project.path().join("b/chart.png");
        for (path, bytes) in [(&a, b"a"), (&b, b"b")] {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        let media = [
            a.to_string_lossy().into_owned(),
            b.to_string_lossy().into_owned(),
        ];
        store_delivered_copies(tenant.path(), &key(), &media);

        assert_eq!(
            std::fs::read(delivered_copy_path(tenant.path(), &key(), &media[0])).unwrap(),
            b"a"
        );
        assert_eq!(
            std::fs::read(delivered_copy_path(tenant.path(), &key(), &media[1])).unwrap(),
            b"b"
        );
    }

    #[test]
    fn should_skip_an_unreadable_path_without_failing_the_others() {
        let tenant = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let present = project.path().join("present.txt");
        std::fs::write(&present, b"ok").unwrap();
        let missing = project
            .path()
            .join("missing.txt")
            .to_string_lossy()
            .into_owned();
        let media = [missing.clone(), present.to_string_lossy().into_owned()];

        store_delivered_copies(tenant.path(), &key(), &media);

        assert!(!delivered_copy_path(tenant.path(), &key(), &missing).exists());
        assert!(delivered_copy_path(tenant.path(), &key(), &media[1]).is_file());
    }
}
