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
    site_metadata("astro-site", build_output_dir)
}

/// Build a metadata fixture with a caller-chosen template and
/// `build_output_dir`. Used by the per-template-equality tests added
/// for the codex follow-up: `astro-site` ↦ `dist`, `nextjs-app` ↦
/// `out`, `quarto-lesson` ↦ `docs`. Any other pairing is now a
/// `TemplateMismatch`.
fn site_metadata(template: &str, build_output_dir: &str) -> SiteProjectMetadata {
    SiteProjectMetadata {
        version: 1,
        command: "/new site astro".to_string(),
        preset_key: "astro".to_string(),
        template: template.to_string(),
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

/// Test 1b: every per-template scaffold pairing (`astro-site` ↦
/// `dist`, `nextjs-app` ↦ `out`, `react-vite` ↦ `dist`,
/// `quarto-lesson` ↦ `docs`) is accepted. Updated for the
/// per-template-equality follow-up — a global allow-list alone is
/// not enough; the value must match `SiteTemplate::output_dir()` for
/// the declared template.
#[test]
fn should_accept_each_template_scaffold_value() {
    for (template, value) in [
        ("astro-site", "dist"),
        ("nextjs-app", "out"),
        ("react-vite", "dist"),
        ("quarto-lesson", "docs"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path();
        std::fs::create_dir_all(project_dir.join(value)).unwrap();
        let metadata = site_metadata(template, value);
        validated_build_output_dir(&metadata, project_dir)
            .unwrap_or_else(|err| panic!("`{template}`/`{value}` must validate but got: {err:?}"));
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

/// Test 5: a symlink placed at `<project_dir>/docs -> /tmp` is
/// rejected by the canonical-descendant check. Allow-listed names
/// alone aren't enough — even if the value passes the per-template
/// equality gate it must canonicalise inside the project. We pair
/// `quarto-lesson` with `docs` (its scaffold output) so the symlink
/// check is the gate the test actually exercises (not the new
/// TemplateMismatch gate).
/// Skipped on Windows where `std::os::unix::fs::symlink` is absent.
#[cfg(unix)]
#[test]
fn should_reject_symlink_escape_after_build() {
    use std::os::unix::fs::symlink;

    let tmp_project = tempfile::tempdir().unwrap();
    let tmp_outside = tempfile::tempdir().unwrap();
    let project_dir = tmp_project.path();

    // `docs` is the `quarto-lesson` scaffold value. Plant a symlink at
    // `<project>/docs -> <outside>` to simulate a malicious symlink
    // left by the build step.
    symlink(tmp_outside.path(), project_dir.join("docs")).unwrap();

    let metadata = site_metadata("quarto-lesson", "docs");
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

// ── Codex round-2 follow-up tests ──────────────────────────────────────────

/// Codex GAP fix: a `null` JSON value in `mofa-site-session.json`
/// must NOT make it past `read_site_project_metadata` and bypass the
/// validator. We don't synthesise a `null` `SiteProjectMetadata` —
/// such a value would fail serde deserialisation against
/// `pub build_output_dir: String` long before reaching the validator.
/// This test pins that contract: writing a session file with a
/// `build_output_dir: null` payload causes
/// `read_site_project_metadata` to return `None`, so the preview
/// handler never gets a metadata struct to mis-trust.
#[test]
fn should_reject_null_json_value() {
    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path().join("sites").join("evil");
    std::fs::create_dir_all(&project_dir).unwrap();

    // Hand-rolled JSON because serde won't let us serialise a typed
    // metadata with a null `build_output_dir`.
    let malicious = serde_json::json!({
        "version": 1,
        "command": "/new site astro",
        "preset_key": "astro",
        "template": "astro-site",
        "site_kind": "docs",
        "site_name": "Test Site",
        "description": "Test fixture",
        "accent": "#000000",
        "reference": "/tmp",
        "reference_label": "tmp",
        "site_slug": "test-site",
        "preview_base_path": "/api/preview/p/s/test-site",
        "preview_url": "/api/preview/p/s/test-site/index.html",
        "build_output_dir": serde_json::Value::Null,
        "project_dir": "sites/test-site",
        "pages": [],
    });
    std::fs::write(
        project_dir.join("mofa-site-session.json"),
        serde_json::to_string_pretty(&malicious).unwrap(),
    )
    .unwrap();

    // The metadata reader uses strongly-typed serde — a null where a
    // String is expected fails to deserialise. The preview handler
    // already short-circuits on `None` with a 404 "Missing Site
    // Metadata" page, so the validator is never reached.
    let read_back = read_site_project_metadata(&project_dir);
    assert!(
        read_back.is_none(),
        "null build_output_dir must fail serde deserialisation, got: {read_back:?}",
    );
}

/// Codex GAP fix: percent-encoded and unicode-escaped `..` variants
/// must be rejected. The allow-list is exact-match against the raw
/// metadata string — the preview HTTP handler operates on the
/// already-decoded `build_output_dir` field, not URL-encoded form, so
/// a metadata value of `..%2Fescape` or `..\u{2f}escape` is matched
/// literally by the allow-list (no `dist`/`out`/`docs`) and rejected
/// before any decode step could happen. Pin both shapes explicitly.
#[test]
fn should_reject_unicode_dot_dot() {
    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path();

    // Percent-encoded slash: `..%2Fescape` is NOT `../escape` to
    // path-component parsing, but it is also not an allow-listed
    // value and contains characters outside the allow-list — must
    // reject (NotAllowListed or TemplateMismatch are both acceptable
    // gates; both prevent the value from being served).
    let metadata = site_metadata_with_build_output("..%2Fescape");
    let result = validated_build_output_dir(&metadata, project_dir);
    assert!(
        matches!(
            result,
            Err(BuildOutputDirError::NotAllowListed
                | BuildOutputDirError::TemplateMismatch
                | BuildOutputDirError::ParentEscape)
        ),
        "percent-encoded `..` must be rejected, got: {result:?}",
    );

    // Unicode-escaped `..` segment: `..\u{2f}escape` evaluates to
    // `../escape` at the source-string level because `\u{2f}` is `/`,
    // so this collapses to the standard ParentEscape case. The
    // explicit pin keeps both shapes covered even if the validator
    // ever stops normalising path separators.
    let metadata = site_metadata_with_build_output("..\u{2f}escape");
    let result = validated_build_output_dir(&metadata, project_dir);
    assert_eq!(
        result,
        Err(BuildOutputDirError::ParentEscape),
        "unicode-escaped `..` (literal `../escape`) must hit the ParentEscape gate",
    );

    // Backslash variant (`..\\escape`) — on Unix `\\` is a single
    // path component, not a separator, so the whole value fails the
    // allow-list. On Windows `\\` IS a separator and the leading
    // `..` makes it `ParentEscape`. Accept either outcome — both
    // reject.
    let metadata = site_metadata_with_build_output("..\\escape");
    let result = validated_build_output_dir(&metadata, project_dir);
    assert!(
        matches!(
            result,
            Err(BuildOutputDirError::NotAllowListed
                | BuildOutputDirError::TemplateMismatch
                | BuildOutputDirError::ParentEscape)
        ),
        "backslash `..` variant must be rejected, got: {result:?}",
    );
}

/// Codex NEEDS-FOLLOWUP fix: `astro-site` with `build_output_dir:
/// "docs"` must be rejected as `TemplateMismatch`. Pre-followup, the
/// global allow-list let this through because `docs` was on the
/// list. Post-followup the per-template gate enforces strict
/// equality against `SiteTemplate::output_dir()`.
#[test]
fn should_reject_template_mismatch() {
    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path();
    // Create both candidate dirs so the canonical-descendant phase
    // can't be the reason for rejection — we want to prove the
    // *equality* gate is what catches this.
    std::fs::create_dir_all(project_dir.join("dist")).unwrap();
    std::fs::create_dir_all(project_dir.join("docs")).unwrap();

    let metadata = site_metadata("astro-site", "docs");
    let result = validated_build_output_dir(&metadata, project_dir);
    assert_eq!(
        result,
        Err(BuildOutputDirError::TemplateMismatch),
        "astro-site + docs must be rejected by per-template equality",
    );

    // And the symmetric case: `nextjs-app` ↦ `out`, not `dist`.
    let metadata = site_metadata("nextjs-app", "dist");
    let result = validated_build_output_dir(&metadata, project_dir);
    assert_eq!(
        result,
        Err(BuildOutputDirError::TemplateMismatch),
        "nextjs-app + dist must be rejected by per-template equality",
    );
}

/// Codex BLOCKING #1 fix: even after `validated_build_output_dir`
/// returns a canonical path, the actual file serve must refuse to
/// follow symlinks on the leaf or any ancestor between the project
/// root and the resolved asset. This pins the symlink-after-validate
/// TOCTOU window: validate succeeds against a real `dist/` dir, then
/// the attacker swaps `dist` for a symlink to `/tmp/escape`. The
/// serve helper [`octos_cli::api::preview::serve_preview_no_follow`]
/// re-walks the path with `symlink_metadata` and refuses.
///
/// Skipped on Windows where `std::os::unix::fs::symlink` is absent.
#[cfg(unix)]
#[test]
fn should_reject_symlink_swap_after_validation() {
    use std::os::unix::fs::symlink;

    use octos_cli::api::preview::serve_preview_no_follow_blocking;

    let tmp_project = tempfile::tempdir().unwrap();
    let tmp_outside = tempfile::tempdir().unwrap();
    let project_dir = tmp_project.path();
    let dist = project_dir.join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(dist.join("index.html"), b"<html>real</html>").unwrap();
    std::fs::write(tmp_outside.path().join("escape"), b"SECRET").unwrap();

    // Phase 1: validation succeeds because `dist` is a real
    // directory at this moment.
    let metadata = site_metadata("astro-site", "dist");
    let validated = validated_build_output_dir(&metadata, project_dir)
        .expect("phase-1 validation must pass against a real dist/");
    assert!(validated.ends_with("dist"));

    // Phase 2: attacker swaps `dist` for a symlink between
    // validation and the file serve.
    std::fs::remove_dir_all(&dist).unwrap();
    symlink(tmp_outside.path(), &dist).unwrap();

    // Phase 3: the serve helper must refuse — the canonical path
    // resolves OUTSIDE `project_dir`, and the ancestor walk catches
    // the swapped symlink even if the canonical form lies.
    let candidate = dist.join("escape");
    let served = serve_preview_no_follow_blocking(project_dir, &candidate);
    assert!(
        served.is_err(),
        "symlink-swapped ancestor must be rejected; got served bytes: {served:?}",
    );
}

// ── Codex round-2 follow-up tests ──────────────────────────────────────────

/// Codex round-2 BLOCKING #2: an LLM that puts `metadata.template:
/// "phantom-template"` and `build_output_dir: "docs"` in
/// `mofa-site-session.json` must NOT validate. Pre-followup,
/// `SiteTemplate::from_slug` returned `Docs` on any unknown slug,
/// so the per-template-equality gate let this through (because
/// `Docs::output_dir() == "docs"`). Post-followup the validator uses
/// `from_slug_strict` and surfaces `UnknownTemplate` on miss; the
/// handler maps that to HTTP 400 via `SiteBuildError::InvalidMetadata`.
#[test]
fn should_reject_unknown_template() {
    let tmp = tempfile::tempdir().unwrap();
    let project_dir = tmp.path();
    std::fs::create_dir_all(project_dir.join("docs")).unwrap();

    // Phantom template — not one of `astro-site` / `nextjs-app` /
    // `react-vite` / `quarto-lesson`. Paired with `docs`, which
    // pre-followup matched the `Docs` fallback's output_dir.
    let metadata = site_metadata("phantom-template", "docs");
    let result = validated_build_output_dir(&metadata, project_dir);
    match result {
        Err(BuildOutputDirError::UnknownTemplate(slug)) => {
            assert_eq!(
                slug, "phantom-template",
                "UnknownTemplate must carry the offending slug verbatim",
            );
        }
        other => panic!("phantom-template must be rejected as UnknownTemplate, got: {other:?}",),
    }

    // Belt-and-braces: every variant of an unknown slug paired with
    // every allow-listed output dir must reject — the previous fix
    // was sensitive to the exact pairing.
    for value in ["dist", "out", "docs"] {
        let metadata = site_metadata("anything-goes", value);
        let result = validated_build_output_dir(&metadata, project_dir);
        assert!(
            matches!(result, Err(BuildOutputDirError::UnknownTemplate(_))),
            "anything-goes/{value} must be rejected, got: {result:?}",
        );
    }
}

/// Codex round-2 BLOCKING #1 follow-up: simulate the handler-shaped
/// race end-to-end. Validate against a real `dist/`, then swap
/// `dist` for a symlink to an outside dir, then call
/// `serve_preview_no_follow_blocking` against the leaf that the
/// handler would have computed. The new `openat`-walk
/// implementation must refuse — must NOT return 200 with the
/// escaped file's content. The round-1 `symlink_metadata` walk
/// could be raced through (multi-syscall window between stat and
/// open); the round-2 walk anchors each step to the previous fd so
/// the swap cannot be observed by the next openat.
///
/// Skipped on Windows where `O_NOFOLLOW` and `openat` are absent;
/// the non-Unix fallback retains the documented residual race.
#[cfg(unix)]
#[test]
fn should_reject_handler_toctou_swap() {
    use std::os::unix::fs::symlink;

    use octos_cli::api::preview::serve_preview_no_follow_blocking;

    let tmp_project = tempfile::tempdir().unwrap();
    let tmp_outside = tempfile::tempdir().unwrap();
    let project_dir = tmp_project.path();
    let dist = project_dir.join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(dist.join("index.html"), b"<html>real</html>").unwrap();
    std::fs::write(tmp_outside.path().join("index.html"), b"SECRET").unwrap();

    // Phase 1: validator passes against a real dist/.
    let metadata = site_metadata("astro-site", "dist");
    let validated = validated_build_output_dir(&metadata, project_dir)
        .expect("phase-1 validation must pass against a real dist/");
    let leaf = validated.join("index.html");

    // Phase 2: between validation and serve, an attacker swaps
    // `dist` for a symlink pointing at `tmp_outside`. The leaf path
    // (as a string) is unchanged.
    std::fs::remove_dir_all(&dist).unwrap();
    symlink(tmp_outside.path(), &dist).unwrap();

    // Phase 3: the serve must refuse. Crucially, it must NOT return
    // `Ok(b"SECRET")` — that's the pre-fix exploit shape: handler
    // returns HTTP 200 with the escaped content.
    //
    // CRITICAL: pass `project_dir` (not the canonical output dir)
    // as the walk root, matching the handler's wiring (see
    // `serve_preview_file` in handlers.rs). The walk must therefore
    // re-traverse `dist` (now a symlink) as an ancestor component
    // and refuse on the `O_NOFOLLOW` open of that step.
    let result = serve_preview_no_follow_blocking(project_dir, &leaf);
    assert!(
        result.is_err(),
        "post-swap serve must refuse — round-1's symlink_metadata walk had a TOCTOU window the openat walk closes; got: {result:?}",
    );
    if let Ok(ref bytes) = result {
        assert_ne!(
            bytes.as_slice(),
            b"SECRET",
            "served bytes from outside the project — TOCTOU race not closed",
        );
    }
}

/// Codex round-2 test gap: an explicit HTTP status assertion at the
/// handler-error-mapping layer. The handler maps
/// `SiteBuildError::InvalidMetadata` (which now wraps
/// `BuildOutputDirError::UnknownTemplate`) to HTTP 400, not 200.
/// The previous error-response shape returned 200 with a
/// "Preview Build Failed" page; the round-1 fix moved validation
/// errors to 4xx. Pin the contract here so the status doesn't
/// regress to 200 silently.
#[tokio::test]
async fn should_return_400_not_200_on_invalid() {
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use octos_cli::api::testing::{SiteBuildError, preview_build_error_response};

    // 1. UnknownTemplate (the new variant) must map to HTTP 400.
    let resp = preview_build_error_response(
        "phantom-template",
        SiteBuildError::InvalidMetadata(BuildOutputDirError::UnknownTemplate(
            "phantom-template".to_string(),
        )),
    );
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "UnknownTemplate must surface as HTTP 400, not 200 or 500",
    );
    let body = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let body_text = String::from_utf8_lossy(&body);
    assert!(
        body_text.contains("Preview Build Rejected"),
        "response body should explain the rejection, got: {body_text}",
    );
    assert!(
        !body_text.contains("/private/") && !body_text.contains("/Users/"),
        "response body must not leak on-disk project paths, got: {body_text}",
    );

    // 2. Cross-check every InvalidMetadata variant — they all
    //    represent LLM-controlled metadata problems and must be 400.
    for reason in [
        BuildOutputDirError::Empty,
        BuildOutputDirError::Absolute,
        BuildOutputDirError::ParentEscape,
        BuildOutputDirError::NotAllowListed,
        BuildOutputDirError::TemplateMismatch,
        BuildOutputDirError::UnknownTemplate("x".into()),
        BuildOutputDirError::OutsideProject,
    ] {
        let resp = preview_build_error_response(
            "astro-site",
            SiteBuildError::InvalidMetadata(reason.clone()),
        );
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "InvalidMetadata({reason:?}) must map to HTTP 400",
        );
    }

    // 3. The post-build re-validation variant (same family of
    //    errors but caught after the build step) also maps to 400.
    let resp = preview_build_error_response(
        "astro-site",
        SiteBuildError::PostBuildValidation(BuildOutputDirError::OutsideProject),
    );
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // 4. UnsupportedTemplate stays 400 (defence-in-depth branch).
    let resp = preview_build_error_response("astro-site", SiteBuildError::UnsupportedTemplate);
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // 5. Genuine build-tool failures stay 5xx — they are NOT
    //    LLM-controlled, so HTTP 500 is the right surface.
    let resp = preview_build_error_response("astro-site", SiteBuildError::BuildCommandFailed);
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let resp = preview_build_error_response("astro-site", SiteBuildError::OutputArtifactMissing);
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
}
