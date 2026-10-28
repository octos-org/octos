//! Cooperative output ownership for native workers in this runtime process.
//! Claims reserve literal paths/subtrees, not filesystem permissions. They do
//! not fence other processes, undeclared writes or detached external writers.

use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde_json::{Value, json};

#[derive(Clone, Debug)]
struct Owner {
    lease: String,
    task_id: Option<String>,
    label: String,
    paths: Vec<PathBuf>,
}

fn registry() -> &'static Mutex<Vec<Owner>> {
    static CLAIMS: OnceLock<Mutex<Vec<Owner>>> = OnceLock::new();
    CLAIMS.get_or_init(|| Mutex::new(Vec::new()))
}

#[derive(Debug)]
pub(crate) struct ClaimError {
    pub code: &'static str,
    pub message: String,
    pub detail: Value,
}

impl ClaimError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: "OUTPUT_CLAIM_INVALID",
            message: message.into(),
            detail: json!({}),
        }
    }

    pub fn result(self) -> super::ToolResult {
        super::ToolResult {
            output: format!("[{}] {} No worker was started.", self.code, self.message),
            success: false,
            structured_metadata: Some(json!({"code": self.code, "detail": self.detail})),
            ..Default::default()
        }
    }
}

/// Captured at tool construction, inherited by descendants; never model-owned.
pub(crate) fn policy_from_env() -> Result<bool, String> {
    match std::env::var("OCTOS_REQUIRE_SHARED_WRITE_CLAIMS") {
        Ok(value) => parse_policy(Some(&value)),
        Err(std::env::VarError::NotPresent) => Ok(false),
        Err(_) => Err("OCTOS_REQUIRE_SHARED_WRITE_CLAIMS must contain valid Unicode".into()),
    }
}

fn parse_policy(value: Option<&str>) -> Result<bool, String> {
    match value.map(str::trim) {
        None | Some("0" | "false") => Ok(false),
        Some("1" | "true") => Ok(true),
        _ => Err("OCTOS_REQUIRE_SHARED_WRITE_CLAIMS must be 0/false or 1/true".into()),
    }
}

/// Only a closed, known read-only allow-list qualifies. Shell, dispatch,
/// discovery/wildcards and unknown plugin tools are conservatively writable.
/// A role name or task prose is never proof of read-only capability.
pub(crate) fn readonly_allowlist(tools: &[String]) -> bool {
    fn readonly(name: &str) -> bool {
        if let Some(group) = super::policy::tool_group_info(name) {
            return group.tools.iter().all(|name| readonly(name));
        }
        matches!(
            name,
            "read_file"
                | "list_dir"
                | "glob"
                | "grep"
                | "view_image"
                | "workspace_diff"
                | "workspace_log"
                | "workspace_show"
        )
    }
    !tools.is_empty() && tools.iter().all(|name| readonly(name))
}

#[cfg(unix)]
fn case_insensitive(root: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    for directory in root.ancestors() {
        let Some(name) = directory.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some((index, character)) = name.char_indices().find(|(_, c)| c.is_ascii_alphabetic())
        else {
            continue;
        };
        let alternate = if character.is_ascii_lowercase() {
            character.to_ascii_uppercase()
        } else {
            character.to_ascii_lowercase()
        };
        let mut swapped = name.to_string();
        swapped.replace_range(index..index + 1, &alternate.to_string());
        let (Ok(original), Ok(other)) = (
            std::fs::metadata(directory),
            std::fs::metadata(directory.with_file_name(swapped)),
        ) else {
            return false;
        };
        return original.dev() == other.dev() && original.ino() == other.ino();
    }
    false
}

#[cfg(not(unix))]
fn case_insensitive(_root: &Path) -> bool {
    cfg!(windows)
}

fn canonical_paths(workspace: &Path, paths: &[String]) -> Result<Vec<PathBuf>, ClaimError> {
    let root = std::fs::canonicalize(workspace)
        .map_err(|e| ClaimError::invalid(format!("Cannot resolve claim workspace: {e}")))?;
    let fold_case = case_insensitive(&root);
    let mut normalized = Vec::new();
    for value in paths {
        let path = Path::new(value);
        if value.trim().is_empty()
            || value
                .chars()
                .any(|c| c.is_control() || "*?[]{}\\".contains(c))
            || path.is_absolute()
            || path
                .components()
                .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
        {
            return Err(ClaimError::invalid(
                "owned_paths must be literal workspace-relative paths without traversal, glob syntax or control characters",
            ));
        }
        let joined = root.join(path);
        // canonicalize_lossy deliberately preserves missing suffixes, but a
        // dangling symlink is not a new literal directory. Reserving it as
        // literal would let its future target be reserved by another worker.
        for ancestor in joined.ancestors().take_while(|p| *p != root) {
            match std::fs::symlink_metadata(ancestor) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    std::fs::canonicalize(ancestor).map_err(|error| {
                        ClaimError::invalid(format!(
                            "Cannot claim through unresolved symlink {}: {error}",
                            ancestor.display()
                        ))
                    })?;
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(ClaimError::invalid(format!(
                        "Cannot inspect claim ancestor {}: {error}",
                        ancestor.display()
                    )));
                }
            }
        }
        let path = octos_core::canonicalize_lossy(&joined);
        if path == root || !path.starts_with(&root) {
            return Err(ClaimError::invalid(
                "Claim must name an output below the workspace, not its root or an outside symlink target",
            ));
        }
        let path = if fold_case {
            PathBuf::from(path.to_string_lossy().to_lowercase())
        } else {
            path
        };
        normalized.push(path);
    }
    normalized.sort();
    normalized.dedup();
    Ok(normalized)
}

/// Ownership follows the actual future, never a supervisor status transition.
#[derive(Debug)]
pub(crate) struct OutputClaimGuard {
    lease: String,
}

impl OutputClaimGuard {
    pub fn acquire(
        workspace: &Path,
        paths: &[String],
        label: &str,
    ) -> Result<Option<Self>, ClaimError> {
        if paths.is_empty() {
            return Ok(None);
        }
        let paths = canonical_paths(workspace, paths)?;
        let mut owners = registry().lock().unwrap_or_else(|e| e.into_inner());
        for owner in owners.iter() {
            for requested in &paths {
                if let Some(held) = owner
                    .paths
                    .iter()
                    .find(|held| requested.starts_with(held) || held.starts_with(requested))
                {
                    let handle = owner.task_id.as_deref().unwrap_or(&owner.lease);
                    return Err(ClaimError {
                        code: "PATH_OWNERSHIP_CONFLICT",
                        message: format!(
                            "Output {} overlaps {} owned by {} ({:?}). Wait for that worker's actual execution to stop, or use a separate output attempt.",
                            requested.display(),
                            held.display(),
                            handle,
                            owner.label
                        ),
                        detail: json!({"owner_task_id": owner.task_id, "owner_lease_id": owner.lease, "owner_label": owner.label, "requested_path": requested, "owned_path": held}),
                    });
                }
            }
        }
        let lease = octos_core::TaskId::new().to_string();
        owners.push(Owner {
            lease: lease.clone(),
            task_id: None,
            label: label.into(),
            paths,
        });
        Ok(Some(Self { lease }))
    }

    pub fn bind_task(&self, task_id: &str) {
        let mut owners = registry().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(owner) = owners.iter_mut().find(|owner| owner.lease == self.lease) {
            owner.task_id = Some(task_id.into());
        }
    }
}

impl Drop for OutputClaimGuard {
    fn drop(&mut self) {
        registry()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|owner| owner.lease != self.lease);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(root: &Path, paths: &[&str]) -> Result<Option<OutputClaimGuard>, ClaimError> {
        OutputClaimGuard::acquire(
            root,
            &paths.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            "test",
        )
    }

    #[test]
    fn output_claim_atomic_overlap_and_release() {
        let dir = tempfile::tempdir().unwrap();
        let held = claim(dir.path(), &["out/a"]).unwrap();
        assert!(claim(dir.path(), &["out/free", "out/a/file"]).is_err());
        let free = claim(dir.path(), &["out/free"]).unwrap();
        let sibling = claim(dir.path(), &["out/ab"]).unwrap();
        assert!(claim(dir.path(), &["out"]).is_err());
        drop((held, free, sibling));
        assert!(claim(dir.path(), &["out"]).is_ok());
    }

    #[test]
    fn output_claim_invalid_paths_have_no_reservations() {
        let dir = tempfile::tempdir().unwrap();
        for path in ["", ".", "../x", "/tmp/x", "out/*", "a/../b", "a\0b"] {
            assert!(claim(dir.path(), &[path]).is_err(), "{path:?}");
        }
        assert!(claim(dir.path(), &["out/./a"]).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn output_claim_symlink_alias_and_escape() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("real")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("alias")).unwrap();
        std::os::unix::fs::symlink(other.path(), dir.path().join("outside")).unwrap();
        let _guard = claim(dir.path(), &["real/new"]).unwrap();
        assert!(claim(dir.path(), &["alias/new/file"]).is_err());
        assert!(claim(dir.path(), &["outside/new"]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn output_claim_dangling_symlink_cannot_reserve_a_distinct_literal_alias() {
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink("future", dir.path().join("alias")).unwrap();
        let held = claim(dir.path(), &["future/out"]).unwrap();
        let rejected = claim(dir.path(), &["alias/out"]);
        assert!(
            rejected.is_err(),
            "dangling symlink alias must fail admission"
        );
        assert_eq!(rejected.unwrap_err().code, "OUTPUT_CLAIM_INVALID");
        std::fs::create_dir(dir.path().join("future")).unwrap();
        assert_eq!(
            claim(dir.path(), &["alias/out"]).unwrap_err().code,
            "PATH_OWNERSHIP_CONFLICT"
        );
        drop(held);
        assert!(claim(dir.path(), &["alias/out"]).is_ok());
    }

    #[test]
    fn output_claim_policy_and_readonly_are_explicit() {
        assert!(!parse_policy(None).unwrap());
        assert!(parse_policy(Some("true")).unwrap());
        assert!(parse_policy(Some("maybe")).is_err());
        assert!(readonly_allowlist(&["read_file".into(), "grep".into()]));
        for names in [
            vec![],
            vec!["bash".into()],
            vec!["group:fs".into()],
            vec!["plugin_read".into()],
            vec!["tool_search".into()],
        ] {
            assert!(!readonly_allowlist(&names));
        }
    }

    #[test]
    fn output_claim_concurrent_admission_has_one_owner() {
        let dir = tempfile::tempdir().unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let results = std::sync::Arc::new(Mutex::new(Vec::new()));
        let threads = (0..2)
            .map(|_| {
                let path = dir.path().to_path_buf();
                let barrier = barrier.clone();
                let results = results.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let guard = claim(&path, &["same/output"]);
                    results.lock().unwrap().push(guard.is_ok());
                    barrier.wait();
                    drop(guard);
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(results.lock().unwrap().iter().filter(|ok| **ok).count(), 1);
    }

    #[test]
    fn output_claim_panic_releases_and_filesystem_case_alias_is_conservative() {
        let dir = tempfile::tempdir().unwrap();
        let _ = std::panic::catch_unwind(|| {
            let _held = claim(dir.path(), &["out"]).unwrap();
            panic!("synthetic owner panic");
        });
        let held = claim(dir.path(), &["out"]).unwrap();
        if case_insensitive(dir.path()) {
            assert!(claim(dir.path(), &["OUT/new"]).is_err());
        }
        drop(held);
    }
}
