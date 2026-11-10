//! Retained path-confinement regressions for [`resolve_tool_path`]:
//! lexical `..` collapse through missing components and symlink paths that
//! must remain lexical for downstream `O_NOFOLLOW` enforcement.

use std::path::Path;

use octos_bus::file_handle::{ResolvedToolPath, ToolPathError, ToolPathScope, resolve_tool_path};
use tempfile::TempDir;

struct Rig {
    workspace: TempDir,
}

impl Rig {
    fn new() -> Self {
        Self {
            workspace: tempfile::tempdir().expect("workspace tmpdir"),
        }
    }

    fn workspace_root(&self) -> &Path {
        self.workspace.path()
    }
}

fn expect_resolved(result: Result<ResolvedToolPath, ToolPathError>) -> ResolvedToolPath {
    result.unwrap_or_else(|err| panic!("resolution must succeed, got {err}"))
}

fn expect_err(result: Result<ResolvedToolPath, ToolPathError>) -> ToolPathError {
    result.expect_err("resolution must fail")
}

// ---------- Extra contract pins ----------

#[test]
fn absolute_input_with_dotdot_through_missing_component_rejected() {
    // Codex review round 4 P2 (2026-05-13): when an absolute input
    // contains `..` after a non-existent component under an allowed
    // root, the previous `canonicalize_lossy` walked back to the
    // closest existing parent and re-attached the suffix VERBATIM,
    // producing a path that still satisfied `starts_with(workspace)`
    // even though it logically escaped. The resolver must lexically
    // collapse `..` BEFORE the containment check so the workspace
    // boundary is honest.
    let rig = Rig::new();
    let workspace = rig.workspace_root();
    let outside = tempfile::tempdir().expect("outside");
    std::fs::write(outside.path().join("secret.txt"), b"escape").unwrap();

    // Input: <workspace>/missing/../../<outside>/secret.txt
    // Lexically normalises to <outside-parent>/secret.txt, which
    // lies outside both the workspace and the upload tmpdir. The
    // resolver must reject as OutsideAllowedRoots.
    let workspace_parent = workspace.parent().expect("workspace parent");
    let traverse = format!(
        "{}/missing/../../{}/secret.txt",
        workspace.display(),
        outside
            .path()
            .strip_prefix(workspace_parent)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| outside.path().display().to_string()),
    );
    let err = expect_err(resolve_tool_path(workspace, None, &traverse));
    assert_eq!(
        err,
        ToolPathError::OutsideAllowedRoots,
        "dotdot through missing component must not be silently accepted as workspace-internal"
    );
}

#[cfg(unix)]
#[test]
fn workspace_relative_symlink_resolution_is_lexical_not_canonical() {
    // Security regression pin (codex review, 2026-05-13): the
    // workspace-relative branch must NOT canonicalise the result.
    // File tools layer `O_NOFOLLOW` over the resolved path; if the
    // resolver canonicalised it would silently translate
    // `workspace/secret -> /etc/passwd` into `/etc/passwd` and the
    // open-time gate would then see a plain file instead of a
    // symlink. The contract is: workspace scope returns the lexical
    // workspace path, the tool's open-time gate is responsible for
    // refusing symlinks.
    let rig = Rig::new();
    let workspace = rig.workspace_root();
    let outside = tempfile::tempdir().expect("outside tmpdir");
    std::fs::write(outside.path().join("passwd"), b"root:x:0:0").unwrap();
    // workspace/secret -> outside/passwd
    let link = workspace.join("secret");
    std::os::unix::fs::symlink(outside.path().join("passwd"), &link).unwrap();

    let resolved = expect_resolved(resolve_tool_path(workspace, None, "secret"));
    assert_eq!(resolved.scope, ToolPathScope::Workspace);
    // The resolver must NOT have followed the symlink — the resolved
    // absolute must still be the workspace path (lexical), not the
    // outside target.
    assert_eq!(resolved.absolute, workspace.join("secret"));
    let outside_path = outside.path().join("passwd");
    assert_ne!(
        resolved.absolute, outside_path,
        "resolver must not follow symlinks for workspace-relative paths"
    );
}
