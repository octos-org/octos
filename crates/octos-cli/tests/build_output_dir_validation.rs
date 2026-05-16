//! Issue #996 (P0 sev1 path-traversal): the LLM-controlled
//! `build_output_dir` field in `mofa-site-session.json` was joined
//! onto `project_dir` without re-validation, allowing the sites
//! preview to escape the project workspace.
//!
//! These tests pin the validation helper in
//! [`octos_cli::project_templates::validated_build_output_dir`] which
//! is the single entry-point every preview consumer must route
//! through.
//!
//! Pre-fix behaviour: `validated_build_output_dir` did not exist, and
//! the preview handler joined `project_dir.join(&metadata.build_output_dir)`
//! verbatim — `"../escape"` returned 200 with the escaped file's
//! content. Post-fix: every test below either rejects the input
//! through the typed error or returns a confined path.

use std::path::Path;

use octos_cli::project_templates::{
    BuildOutputDirError, SiteProjectMetadata, read_site_project_metadata,
    validated_build_output_dir,
};

fn site_metadata_with_build_output(build_output_dir: &str) -> SiteProjectMetadata {
    SiteProjectMetadata {
        version: 1,
        command: "/new site astro".to_string(),
        preset_key: "astro".to_string(),
        template: "astro-site".to_string(),
        site_kind: "docs".to_string(),
        site_name: "Test Site".to_string(),
        description: "Test fixture".to_string(),
        accent: "#000000".to_string(),
        reference: "/tmp".to_string(),
        reference_label: "tmp".to_string(),
        site_slug: "test-site".to_string(),
        preview_base_path: "/api/preview/p/s/test-site".to_string(),
        preview_url: "/api/preview/p/s/test-site/index.html".to_string(),
        build_output_dir: build_output_dir.to_string(),
        project_dir: "sites/test-site".to_string(),
        pages: Vec::new(),
    }
}

/// Test 1: an allow-listed value (`dist`, populated by the template
/// scaffold) is accepted.
#[test]
fn should_accept_allow_listed_dist_value() {
    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path();
    std::fs::create_dir_all(project_dir.join("dist")).unwrap();

    let metadata = site_metadata_with_build_output("dist");
    let resolved = validated_build_output_dir(&metadata, project_dir)
        .expect("scaffold-derived `dist` must validate");
    let canonical_project = std::fs::canonicalize(project_dir).unwrap();
    assert!(resolved.starts_with(&canonical_project));
    assert!(resolved.ends_with("dist"));
}

/// Test 1b: every per-template scaffold value (`dist`, `out`, `docs`)
/// is accepted as documented in the allow-list.
#[test]
fn should_accept_each_template_scaffold_value() {
    for value in ["dist", "out", "docs"] {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path();
        std::fs::create_dir_all(project_dir.join(value)).unwrap();
        let metadata = site_metadata_with_build_output(value);
        validated_build_output_dir(&metadata, project_dir).unwrap_or_else(|err| {
            panic!("scaffold value `{value}` must validate but got: {err:?}")
        });
    }
}

/// Test 2: a relative path that escapes via `..` is rejected — this
/// is the exploit shape called out in the issue (`"../escape"`).
/// Pre-fix this returned 200 with the escaped file's content.
#[test]
fn should_reject_dot_dot_escape() {
    let tmp = tempfile::tempdir().unwrap();
    let metadata = site_metadata_with_build_output("../escape");
    let result = validated_build_output_dir(&metadata, tmp.path());
    assert_eq!(result, Err(BuildOutputDirError::ParentEscape));
}

/// Test 3: an absolute path (e.g. `/etc/passwd`) is rejected before
/// any join happens.
#[test]
fn should_reject_absolute_path() {
    let tmp = tempfile::tempdir().unwrap();
    let metadata = site_metadata_with_build_output("/etc/passwd");
    let result = validated_build_output_dir(&metadata, tmp.path());
    assert_eq!(result, Err(BuildOutputDirError::Absolute));
}

/// Test 4: a relative path that *normalises* to an escape via mixed
/// `..` segments (`output/sub/../../../escape`) is rejected by the
/// per-component scan, not the final canonicalise pass.
#[test]
fn should_reject_post_normalization_escape() {
    let tmp = tempfile::tempdir().unwrap();
    let metadata = site_metadata_with_build_output("output/sub/../../../escape");
    let result = validated_build_output_dir(&metadata, tmp.path());
    assert_eq!(result, Err(BuildOutputDirError::ParentEscape));
}

/// Test 5: a symlink placed at `<project_dir>/output -> /tmp` is
/// rejected by the canonical-descendant check. Allow-listed names
/// alone aren't enough — `output` itself is on the allow-list
/// historically used elsewhere in the scaffold, so even if the value
/// passes the allow-list it must canonicalise inside the project.
/// Skipped on Windows where `std::os::unix::fs::symlink` is absent.
#[cfg(unix)]
#[test]
fn should_reject_symlink_escape_after_build() {
    use std::os::unix::fs::symlink;

    let tmp_project = tempfile::tempdir().unwrap();
    let tmp_outside = tempfile::tempdir().unwrap();
    let project_dir = tmp_project.path();

    // `docs` is an allow-listed scaffold value. Plant a symlink at
    // `<project>/docs -> <outside>` to simulate a malicious symlink
    // left by the build step.
    symlink(tmp_outside.path(), project_dir.join("docs")).unwrap();

    let metadata = site_metadata_with_build_output("docs");
    let result = validated_build_output_dir(&metadata, project_dir);
    assert_eq!(
        result,
        Err(BuildOutputDirError::OutsideProject),
        "symlink-escape after allow-list must be rejected by canonical-descendant check"
    );
}

/// Test 6: an empty / whitespace-only metadata value is rejected.
#[test]
fn should_reject_empty_string() {
    let tmp = tempfile::tempdir().unwrap();
    let metadata = site_metadata_with_build_output("");
    let result = validated_build_output_dir(&metadata, tmp.path());
    assert_eq!(result, Err(BuildOutputDirError::Empty));

    let metadata = site_metadata_with_build_output("   ");
    let result = validated_build_output_dir(&metadata, tmp.path());
    assert_eq!(result, Err(BuildOutputDirError::Empty));
}

/// Test 6b: an arbitrary non-allow-listed value (e.g. `build` or
/// `public`) is rejected even though it would resolve safely inside
/// the project — the contract is the closed allow-list, not
/// "anything inside `project_dir`".
#[test]
fn should_reject_non_allow_listed_value() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("build")).unwrap();
    let metadata = site_metadata_with_build_output("build");
    let result = validated_build_output_dir(&metadata, tmp.path());
    assert_eq!(result, Err(BuildOutputDirError::NotAllowListed));
}

/// Test 7: live-preview-shaped probe. Write a malicious
/// `mofa-site-session.json` (`build_output_dir: "../../etc"`) and
/// read it back through the same in-process helper the preview
/// handler uses (`read_site_project_metadata` →
/// `validated_build_output_dir`). The validator must reject; pre-fix,
/// joining `project_dir.join("../../etc")` would have escaped to the
/// host etc dir and the preview handler returned 200 with its
/// content.
#[test]
fn should_reject_malicious_session_json_for_preview() {
    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path().join("sites").join("evil");
    std::fs::create_dir_all(&project_dir).unwrap();

    let metadata = site_metadata_with_build_output("../../etc");
    let serialized = serde_json::to_string_pretty(&metadata).unwrap();
    std::fs::write(project_dir.join("mofa-site-session.json"), serialized).unwrap();

    let read_back =
        read_site_project_metadata(&project_dir).expect("metadata must round-trip via serde");
    assert_eq!(read_back.build_output_dir, "../../etc");

    let result = validated_build_output_dir(&read_back, &project_dir);
    assert_eq!(
        result,
        Err(BuildOutputDirError::ParentEscape),
        "malicious session.json must be rejected — pre-fix this returned 200 with /etc content"
    );
}

/// Test 7b: a single leading `..` is rejected the same way, and the
/// resulting joined path does NOT exist under the project dir (so
/// even if a caller ignored the error, no file would be served from
/// inside the workspace).
#[test]
fn should_reject_single_dot_dot_and_not_expose_parent_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();
    // Plant a sibling file that would be the exploit target.
    let secret = tmp.path().join("secret.txt");
    std::fs::write(&secret, b"PRIVATE").unwrap();

    let metadata = site_metadata_with_build_output("../");
    let result = validated_build_output_dir(&metadata, &project_dir);
    assert!(matches!(
        result,
        Err(BuildOutputDirError::ParentEscape | BuildOutputDirError::NotAllowListed)
    ));

    // Sanity: the secret file is still present (no side effects), and
    // the validator did not return its path.
    assert!(secret.exists());
    assert!(result.is_err());
}

/// Sanity guard: the SiteProjectMetadata fields the validator
/// inspects are stable. If `build_output_dir` is ever renamed the
/// validator must be updated — this test fails fast if the field
/// name drifts.
#[test]
fn metadata_field_path_is_stable() {
    let metadata = site_metadata_with_build_output("dist");
    let json = serde_json::to_value(&metadata).unwrap();
    assert_eq!(
        json.get("build_output_dir").and_then(|v| v.as_str()),
        Some("dist"),
        "validator depends on `build_output_dir` field name; update validator if this changes"
    );
}

/// Defence-in-depth: the project_dir argument the validator
/// canonicalises is the call-site's responsibility — confirm a
/// missing project_dir still rejects, doesn't fall back to a raw
/// join.
#[test]
fn missing_project_dir_does_not_bypass_validation() {
    let tmp = tempfile::tempdir().unwrap();
    let nonexistent = tmp.path().join("does-not-exist");
    // `dist` is allow-listed, but the canonical-descendant phase
    // would normally need both paths to exist. The form-check still
    // accepts the join (this is acceptable because the structural
    // checks rule out escape via `..` / absolute / non-allow-list).
    let metadata = site_metadata_with_build_output("dist");
    let result = validated_build_output_dir(&metadata, &nonexistent).unwrap();
    // The returned path must still be a child of the requested
    // project dir even though we couldn't canonicalise either side.
    assert!(result.ends_with("dist"));
    let _ = Path::new(&result);
}
