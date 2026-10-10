//! Read-only Git snapshots of the server-bound session workspace.
use super::*;
use crate::contracts::diff::parse_unified_diff_preview_files;
use octos_core::ui_protocol::{
    DiffPreview, DiffPreviewGetParams, DiffWorkspaceGetParams, PreviewId,
};
use std::process::Stdio;
use tokio::io::AsyncReadExt;

const MAX_BYTES: usize = 2 * 1024 * 1024;
const MAX_FILES: usize = 256;
const MAX_PATCH_LINES: usize = 10_000;

fn count_patch_lines(patch: &[u8], used: &mut usize) -> Result<(), String> {
    *used += patch.iter().filter(|b| **b == b'\n').count();
    if *used > MAX_PATCH_LINES {
        return Err(
            "workspace diff exceeds 10000 patch lines; narrow the workspace and retry".into(),
        );
    }
    Ok(())
}

pub(super) async fn rpc(
    state: &Arc<AppState>,
    contracts: &UiProtocolContractStores,
    request: &RpcRequest<Value>,
    connection_profile: Option<&str>,
) -> Result<Value, RpcError> {
    let params: DiffWorkspaceGetParams = parse_raw_params(request)?;
    let profile = validate_session_scope(&params.session_id, None, connection_profile)?;
    let runtime = resolve_session_profile_runtime(state, profile.as_deref())
        .ok_or_else(|| runtime_unavailable_error("profile runtime unavailable"))?;
    let workspace =
        session_workspace_root_for_profile(Some(&runtime.profile_id), &params.session_id)
            .ok_or_else(|| {
                RpcError::invalid_request("open the session before reading its workspace changes")
            })?;
    let preview = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        collect(&workspace, params.session_id.clone()),
    )
    .await
    .map_err(|_| {
        RpcError::invalid_request("workspace diff timed out; narrow the workspace and retry")
    })?
    .map_err(RpcError::invalid_request)?;
    let preview_id = preview.preview_id.clone();
    let store = contracts.diff_previews(Some(&runtime.data_dir));
    store.insert_with_snapshot(preview, None);
    serde_json::to_value(store.get(DiffPreviewGetParams {
        session_id: params.session_id,
        preview_id,
    })?)
    .map_err(|e| RpcError::internal_error(e.to_string()))
}

// No shell, external diff drivers, textconv or fsmonitor hooks. Bounded pipes
// and kill-on-drop also bound resource use when the request times out.
async fn git(root: &Path, args: &[&str], remaining: &mut usize) -> Result<(bool, Vec<u8>), String> {
    let mut child = tokio::process::Command::new("git")
        .args([
            "--no-pager",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.quotePath=false",
        ])
        .args(args)
        .current_dir(root)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("cannot read Git changes: {e}"))?;
    let mut bytes = Vec::new();
    child
        .stdout
        .take()
        .ok_or("missing Git output")?
        .take(*remaining as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|e| e.to_string())?;
    if bytes.len() > *remaining {
        return Err("workspace diff exceeds 2 MiB; narrow the workspace and retry".into());
    }
    *remaining -= bytes.len();
    let status = child.wait().await.map_err(|e| e.to_string())?;
    Ok((status.success(), bytes))
}

async fn collect(root: &Path, session_id: SessionKey) -> Result<DiffPreview, String> {
    let mut remaining = MAX_BYTES;
    let (repo, _) = git(root, &["rev-parse", "--show-toplevel"], &mut remaining).await?;
    if !repo {
        return Err("this workspace is not a Git repository".into());
    }
    let (has_head, _) = git(root, &["rev-parse", "--verify", "HEAD"], &mut remaining).await?;
    // The empty tree works for unborn repositories and includes staged new files.
    let empty_tree;
    let base = if has_head {
        "HEAD"
    } else {
        let (ok, hash) = git(
            root,
            &["hash-object", "-t", "tree", "--stdin"],
            &mut remaining,
        )
        .await?;
        if !ok {
            return Err("Git could not resolve an empty tree".into());
        }
        empty_tree = String::from_utf8(hash).map_err(|_| "invalid Git object ID")?;
        empty_tree.trim()
    };
    let (ok, patch) = git(
        root,
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--no-renames",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "--relative",
            base,
            "--",
            ".",
        ],
        &mut remaining,
    )
    .await?;
    if !ok {
        return Err("Git could not read the workspace diff".into());
    }
    let mut patch_lines = 0;
    count_patch_lines(&patch, &mut patch_lines)?;
    let mut files = parse_unified_diff_preview_files(&String::from_utf8_lossy(&patch))
        .ok_or("Git returned an unsupported filename or patch")?;
    let (ok, untracked) = git(
        root,
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            ".",
        ],
        &mut remaining,
    )
    .await?;
    if !ok {
        return Err("Git could not list untracked files".into());
    }
    for path in untracked.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        if files.len() >= MAX_FILES {
            return Err("workspace diff exceeds 256 files; narrow the workspace and retry".into());
        }
        let path = std::str::from_utf8(path).map_err(|_| "a changed filename is not UTF-8")?;
        let relative = Path::new(path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err("Git returned a path outside the workspace".into());
        }
        // Git's no-index path handles binary files and symlink contents without
        // following a final symlink. Refuse symlinked parents leaving the root.
        let full = root.join(relative);
        let parent = tokio::fs::canonicalize(full.parent().ok_or("invalid file path")?)
            .await
            .map_err(|e| e.to_string())?;
        let canonical_root = tokio::fs::canonicalize(root)
            .await
            .map_err(|e| e.to_string())?;
        if !parent.starts_with(&canonical_root) {
            return Err("changed file is outside the workspace".into());
        }
        let (_, patch) = git(
            root,
            &[
                "diff",
                "--no-index",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--",
                "/dev/null",
                path,
            ],
            &mut remaining,
        )
        .await?;
        count_patch_lines(&patch, &mut patch_lines)?;
        let mut added =
            parse_unified_diff_preview_files(&String::from_utf8_lossy(&patch)).unwrap_or_default();
        let Some(mut file) = added.pop() else {
            return Err(format!("Git could not read {path}"));
        };
        file.path = path.to_owned();
        file.old_path = None;
        files.push(file);
    }
    if files.len() > MAX_FILES {
        return Err("workspace diff exceeds 256 files; narrow the workspace and retry".into());
    }
    for file in &mut files {
        if file.old_path.as_deref() == Some(&file.path) {
            file.old_path = None;
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(DiffPreview {
        session_id,
        preview_id: PreviewId::new(),
        title: Some("Workspace uncommitted changes".into()),
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn git_sync(root: &Path, args: &[&str]) {
        assert!(
            std::process::Command::new("git")
                .current_dir(root)
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
    }
    #[tokio::test]
    async fn workspace_diff_tracks_index_worktree_untracked_and_subfolder() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git_sync(root, &["init"]);
        std::fs::create_dir(root.join("inside")).unwrap();
        std::fs::write(root.join("inside/a space.txt"), "old\n-- old\n").unwrap();
        std::fs::write(root.join("outside.txt"), "old\n").unwrap();
        git_sync(root, &["add", "."]);
        git_sync(
            root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.test",
                "commit",
                "-m",
                "base",
            ],
        );
        std::fs::write(root.join("inside/a space.txt"), "staged\n++ new\n").unwrap();
        git_sync(root, &["add", "."]);
        std::fs::write(root.join("inside/a space.txt"), "unstaged\n++ new\n").unwrap();
        std::fs::write(root.join("outside.txt"), "changed\n").unwrap();
        std::fs::write(root.join("inside/new.txt"), "untracked\n").unwrap();
        std::fs::write(root.join("inside/.gitignore"), "ignored\n").unwrap();
        std::fs::write(root.join("inside/ignored"), "secret\n").unwrap();
        let preview = collect(&root.join("inside"), SessionKey("test".into()))
            .await
            .unwrap();
        assert_eq!(
            preview
                .files
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            [".gitignore", "a space.txt", "new.txt"]
        );
        let lines: Vec<_> = preview.files[1]
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .map(|l| l.content.as_str())
            .collect();
        assert!(lines.contains(&"unstaged"));
        assert!(lines.contains(&"-- old"));
        assert!(lines.contains(&"++ new"));
        // Reading never stages files or changes the index.
        let index = std::process::Command::new("git")
            .current_dir(root)
            .args(["show", ":inside/a space.txt"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8(index.stdout).unwrap(), "staged\n++ new\n");
    }
    #[tokio::test]
    async fn workspace_diff_unborn_empty_binary_and_size_bound() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(collect(root, SessionKey("s".into())).await.is_err());
        git_sync(root, &["init"]);
        assert!(
            collect(root, SessionKey("s".into()))
                .await
                .unwrap()
                .files
                .is_empty()
        );
        std::fs::write(root.join("empty"), "").unwrap();
        std::fs::write(root.join("binary"), b"\0data").unwrap();
        std::fs::write(root.join("新\tfile"), "text\n").unwrap();
        git_sync(root, &["add", "新\tfile"]);
        let preview = collect(root, SessionKey("s".into())).await.unwrap();
        assert_eq!(preview.files.len(), 3);
        std::fs::write(root.join("many-lines"), "x\n".repeat(MAX_PATCH_LINES)).unwrap();
        assert!(
            collect(root, SessionKey("s".into()))
                .await
                .unwrap_err()
                .contains("10000 patch lines")
        );
        std::fs::remove_file(root.join("many-lines")).unwrap();
        assert!(preview.files.iter().any(|f| f.path == "新\tfile"));
        std::fs::write(root.join("large"), "x\n".repeat(MAX_BYTES)).unwrap();
        assert!(
            collect(root, SessionKey("s".into()))
                .await
                .unwrap_err()
                .contains("2 MiB")
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_diff_symlink_content_never_reads_its_external_target() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(
            outside.path().join("private"),
            "NEVER_INCLUDE_TARGET_CONTENT",
        )
        .unwrap();
        git_sync(dir.path(), &["init"]);
        std::os::unix::fs::symlink(outside.path().join("private"), dir.path().join("link"))
            .unwrap();
        let preview = collect(dir.path(), SessionKey("s".into())).await.unwrap();
        assert_eq!(preview.files.len(), 1);
        assert!(
            !serde_json::to_string(&preview)
                .unwrap()
                .contains("NEVER_INCLUDE_TARGET_CONTENT")
        );
    }

    #[test]
    fn workspace_diff_is_raw_and_scoped() {
        use octos_core::ui_protocol::methods::DIFF_WORKSPACE_GET;
        assert!(raw_method_is_dispatched(DIFF_WORKSPACE_GET, false));
        assert!(!session_ingress_callable_method(DIFF_WORKSPACE_GET));
        assert!(
            validate_session_scope(
                &SessionKey::with_profile("other", "api", "session"),
                None,
                Some("mine")
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<DiffWorkspaceGetParams>(json!({"session_id":"s", "cwd":"/"}))
                .is_err()
        );
    }
}
