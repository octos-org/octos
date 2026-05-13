//! Empirical resolution table for the unified [`resolve_tool_path`].
//!
//! Every row in the "Input forms the LLM produces" table from
//! `refactor: unified file-path resolver` is exercised here. If any
//! row regresses, ONE of these tests fails — kills the upload-handle
//! bug class (#586, #857, #930, #931, #932, #933) at source.

use std::path::{Path, PathBuf};

use octos_bus::file_handle::{
    ResolvedToolPath, ToolPathError, ToolPathScope, encode_profile_file_handle,
    encode_tmp_upload_handle, resolve_tool_path, temp_upload_root,
};
use tempfile::TempDir;

/// Stand-up rig for the row-by-row table: a fake workspace, a fake
/// profile root, and a real per-test directory inside the global
/// `temp_upload_root()` (so the canonicalize-based existence check
/// passes without leaking across tests).
struct Rig {
    workspace: TempDir,
    profile: TempDir,
    /// Directory inside `temp_upload_root()` that holds the test's
    /// fake upload payload. Drop cleans it up.
    upload_dir: PathBuf,
}

impl Rig {
    fn new(tag: &str) -> Self {
        let workspace = tempfile::tempdir().expect("workspace tmpdir");
        let profile = tempfile::tempdir().expect("profile tmpdir");

        // Real subdirectory under temp_upload_root() so the
        // canonicalize-based existence checks succeed.
        let upload_root = temp_upload_root();
        std::fs::create_dir_all(&upload_root).expect("upload root");
        let upload_dir = upload_root.join(format!(
            "resolve-tool-path-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        std::fs::create_dir_all(&upload_dir).expect("upload dir");

        Self {
            workspace,
            profile,
            upload_dir,
        }
    }

    fn workspace_root(&self) -> &Path {
        self.workspace.path()
    }

    fn profile_root(&self) -> &Path {
        self.profile.path()
    }

    /// Build a real workspace-relative file and return both its
    /// relative form and the canonicalised absolute path.
    fn make_workspace_file(&self, relative: &str, body: &[u8]) -> PathBuf {
        let abs = self.workspace.path().join(relative);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).expect("workspace parent dir");
        }
        std::fs::write(&abs, body).expect("write workspace file");
        std::fs::canonicalize(&abs).expect("canonicalise workspace file")
    }

    /// Build a real file inside the rig's upload directory and return
    /// its canonicalised absolute path.
    fn make_upload_file(&self, name: &str, body: &[u8]) -> PathBuf {
        let abs = self.upload_dir.join(name);
        std::fs::write(&abs, body).expect("write upload file");
        std::fs::canonicalize(&abs).expect("canonicalise upload file")
    }

    /// Build a real file under the profile root.
    fn make_profile_file(&self, relative: &str, body: &[u8]) -> PathBuf {
        let abs = self.profile.path().join(relative);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).expect("profile parent dir");
        }
        std::fs::write(&abs, body).expect("write profile file");
        std::fs::canonicalize(&abs).expect("canonicalise profile file")
    }

    /// Relative path inside the rig's upload subdir as the file_handle
    /// helpers see it (the bit between `temp_upload_root()` and the
    /// file).
    fn upload_relative(&self, name: &str) -> PathBuf {
        self.upload_dir
            .strip_prefix(temp_upload_root())
            .map(|p| p.join(name))
            .expect("upload dir lies under temp_upload_root")
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.upload_dir);
    }
}

fn expect_resolved(result: Result<ResolvedToolPath, ToolPathError>) -> ResolvedToolPath {
    result.unwrap_or_else(|err| panic!("resolution must succeed, got {err}"))
}

fn expect_err(result: Result<ResolvedToolPath, ToolPathError>) -> ToolPathError {
    result.expect_err("resolution must fail")
}

#[test]
fn row_1_workspace_relative_input() {
    let rig = Rig::new("row1");
    let _abs = rig.make_workspace_file("foo/bar.txt", b"hi");

    let resolved = expect_resolved(resolve_tool_path(
        rig.workspace_root(),
        Some(rig.profile_root()),
        "foo/bar.txt",
    ));
    assert_eq!(resolved.scope, ToolPathScope::Workspace);
    // Workspace-relative resolution returns the LEXICAL workspace path
    // on purpose — file tools layer `O_NOFOLLOW` over the resolved
    // path, and canonicalising here would silently follow symlinks
    // before that gate gets to run. See `row_workspace_keeps_lexical_for_symlink_safety`.
    assert_eq!(resolved.absolute, rig.workspace_root().join("foo/bar.txt"));
}

#[test]
fn row_1b_workspace_relative_writes_for_nonexistent_path() {
    // Write-style tools (`write_file`, `edit_file`) need a workspace
    // path even when the file does not yet exist; the resolver must
    // return the normalised location instead of failing.
    let rig = Rig::new("row1b");
    let resolved = expect_resolved(resolve_tool_path(
        rig.workspace_root(),
        Some(rig.profile_root()),
        "subdir/new-output.txt",
    ));
    assert_eq!(resolved.scope, ToolPathScope::Workspace);
    assert_eq!(
        resolved.absolute,
        rig.workspace_root().join("subdir/new-output.txt")
    );
}

#[test]
fn row_2_absolute_inside_workspace_kept() {
    let rig = Rig::new("row2");
    let abs = rig.make_workspace_file("foo.txt", b"hi");

    let resolved = expect_resolved(resolve_tool_path(
        rig.workspace_root(),
        Some(rig.profile_root()),
        &abs.to_string_lossy(),
    ));
    assert_eq!(resolved.scope, ToolPathScope::Workspace);
    // For absolute paths the resolver collapses macOS firmlinks via
    // `canonicalize_lossy`, so the resolved path is the canonical form.
    // The workspace-relative branch keeps the lexical workspace path —
    // see `row_1_workspace_relative_input`.
    assert_eq!(resolved.absolute, abs);
}

#[test]
fn row_3_absolute_inside_upload_tmpdir_kept() {
    let rig = Rig::new("row3");
    let abs = rig.make_upload_file("019e22ab-cd-real-upload.wav", b"WAV");

    let resolved = expect_resolved(resolve_tool_path(
        rig.workspace_root(),
        Some(rig.profile_root()),
        &abs.to_string_lossy(),
    ));
    assert_eq!(resolved.scope, ToolPathScope::UploadTmpdir);
    assert!(
        resolved
            .absolute
            .starts_with(std::fs::canonicalize(temp_upload_root()).unwrap()),
        "resolved {:?} must canonicalise under {:?}",
        resolved.absolute,
        temp_upload_root()
    );
}

#[test]
fn row_4_three_segment_upload_handle() {
    let rig = Rig::new("row4");
    let abs = rig.make_upload_file("019e22-three-segment.wav", b"WAV");
    let relative = rig.upload_relative("019e22-three-segment.wav");

    let handle = encode_tmp_upload_handle(
        &temp_upload_root().join(&relative),
        Some("019e22-three-segment.wav"),
    )
    .expect("3-segment handle encoded");
    assert!(handle.starts_with("up/"));
    assert!(handle.matches('/').count() >= 2);

    let resolved = expect_resolved(resolve_tool_path(
        rig.workspace_root(),
        Some(rig.profile_root()),
        &handle,
    ));
    assert_eq!(resolved.scope, ToolPathScope::UploadTmpdir);
    // canonicalise inside test to handle /private firmlinks on macOS.
    assert_eq!(resolved.absolute, abs);
}

#[test]
fn row_5_two_segment_upload_handle_no_display() {
    let rig = Rig::new("row5");
    let abs = rig.make_upload_file("019e22-two-segment.wav", b"WAV");
    let relative = rig.upload_relative("019e22-two-segment.wav");

    let full_handle = encode_tmp_upload_handle(
        &temp_upload_root().join(&relative),
        Some("019e22-two-segment.wav"),
    )
    .expect("handle");
    // Strip the trailing display segment — the LLM frequently drops it.
    let payload = full_handle.split('/').nth(1).expect("payload");
    let two_segment = format!("up/{payload}");

    let resolved = expect_resolved(resolve_tool_path(
        rig.workspace_root(),
        Some(rig.profile_root()),
        &two_segment,
    ));
    assert_eq!(resolved.scope, ToolPathScope::UploadTmpdir);
    assert_eq!(resolved.absolute, abs);
}

#[test]
fn row_6_three_segment_profile_handle() {
    let rig = Rig::new("row6");
    let abs = rig.make_profile_file("slides/demo/output/deck.pptx", b"pptx");
    let handle = encode_profile_file_handle(rig.profile_root(), &abs).expect("pf handle");
    assert!(handle.starts_with("pf/"));
    assert!(handle.matches('/').count() >= 2);

    let resolved = expect_resolved(resolve_tool_path(
        rig.workspace_root(),
        Some(rig.profile_root()),
        &handle,
    ));
    assert_eq!(resolved.scope, ToolPathScope::Profile);
    assert_eq!(resolved.absolute, abs);
}

#[test]
fn row_7_two_segment_profile_handle_no_display() {
    let rig = Rig::new("row7");
    let abs = rig.make_profile_file("slides/two-seg/output/deck.pptx", b"pptx");
    let full_handle = encode_profile_file_handle(rig.profile_root(), &abs).expect("pf handle");
    let payload = full_handle.split('/').nth(1).expect("payload");
    let two_segment = format!("pf/{payload}");

    let resolved = expect_resolved(resolve_tool_path(
        rig.workspace_root(),
        Some(rig.profile_root()),
        &two_segment,
    ));
    assert_eq!(resolved.scope, ToolPathScope::Profile);
    assert_eq!(resolved.absolute, abs);
}

#[test]
fn row_8_bare_basename_under_upload_tmpdir() {
    // The server writes uploads as `<temp_upload_root()>/<uuid>.<ext>`.
    // The LLM frequently passes only the basename back. The resolver
    // must locate the on-disk file under the upload tmpdir without
    // mistaking it for a workspace-relative path.
    let upload_root = temp_upload_root();
    std::fs::create_dir_all(&upload_root).unwrap();
    let bare_name = format!(
        "019e22-bare-{}-{}.wav",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    let abs = upload_root.join(&bare_name);
    std::fs::write(&abs, b"WAV").unwrap();
    let canonical = std::fs::canonicalize(&abs).unwrap();

    let rig = Rig::new("row8");
    let resolved = expect_resolved(resolve_tool_path(
        rig.workspace_root(),
        Some(rig.profile_root()),
        &bare_name,
    ));
    assert_eq!(resolved.scope, ToolPathScope::UploadTmpdir);
    assert_eq!(resolved.absolute, canonical);

    let _ = std::fs::remove_file(&abs);
}

#[test]
fn row_9_parent_traversal_rejected() {
    let rig = Rig::new("row9");
    let err = expect_err(resolve_tool_path(
        rig.workspace_root(),
        Some(rig.profile_root()),
        "../../etc/passwd",
    ));
    assert_eq!(err, ToolPathError::Traversal);
}

#[test]
fn row_10_absolute_outside_all_allowed_roots_rejected() {
    let rig = Rig::new("row10");
    // /etc/passwd is a stable target outside every test root. We
    // don't read the file — only require that the resolver refuses.
    let err = expect_err(resolve_tool_path(
        rig.workspace_root(),
        Some(rig.profile_root()),
        "/etc/passwd",
    ));
    assert_eq!(err, ToolPathError::OutsideAllowedRoots);
}

// ---------- Extra contract pins ----------

#[test]
fn workspace_root_absolute_without_profile_still_works() {
    // The most common live call path passes `profile_root = None`
    // (read_file/write_file/etc. don't track the profile root). The
    // resolver must still operate on workspace + upload tmpdir.
    let rig = Rig::new("no-profile");
    let _abs = rig.make_workspace_file("hello.txt", b"x");

    let resolved = expect_resolved(resolve_tool_path(rig.workspace_root(), None, "hello.txt"));
    assert_eq!(resolved.scope, ToolPathScope::Workspace);
    // Lexical workspace path — same reason as `row_1_workspace_relative_input`.
    assert_eq!(resolved.absolute, rig.workspace_root().join("hello.txt"));
}

#[test]
fn profile_handle_without_profile_root_fails() {
    // A `pf/...` handle supplied without a `profile_root` argument is
    // unresolvable. Surface as DecodeFailed so callers can fall back to
    // their own legacy paths if any.
    let rig = Rig::new("no-profile-pf");
    let abs = rig.make_profile_file("slides/demo/deck.pptx", b"x");
    let handle = encode_profile_file_handle(rig.profile_root(), &abs).expect("pf handle");

    let err = expect_err(resolve_tool_path(rig.workspace_root(), None, &handle));
    assert_eq!(err, ToolPathError::DecodeFailed);
}

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
    let rig = Rig::new("dotdot");
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
    let rig = Rig::new("sym-safety");
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

#[cfg(target_os = "macos")]
#[test]
fn macos_firmlink_canonicalisation_for_upload_root() {
    // macOS firmlinks expose `/var/folders/...` and
    // `/private/var/folders/...` as the same directory.
    // `std::fs::canonicalize` collapses to the `/private/...` form, but
    // `temp_upload_root()` returns the un-prefixed `/var/...` path.
    // The unified resolver must accept BOTH forms.
    let rig = Rig::new("firmlink");
    let abs = rig.make_upload_file("firmlink.wav", b"WAV");
    let canon = std::fs::canonicalize(&abs).expect("canonicalise");
    let canon_str = canon.to_string_lossy();
    assert!(
        canon_str.starts_with("/private/var/") || canon_str.starts_with("/var/"),
        "expected macOS tmpdir under /var/folders/, got {canon_str}"
    );

    let resolved = expect_resolved(resolve_tool_path(
        rig.workspace_root(),
        Some(rig.profile_root()),
        &canon.to_string_lossy(),
    ));
    assert_eq!(resolved.scope, ToolPathScope::UploadTmpdir);
}
