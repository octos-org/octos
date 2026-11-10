//! Compact #996 path-confinement representatives for build-output metadata.

use octos_cli::project_templates::{
    BuildOutputDirError, SiteProjectMetadata, validated_build_output_dir,
};

fn site_metadata(template: &str, build_output_dir: &str) -> SiteProjectMetadata {
    SiteProjectMetadata {
        version: 1,
        command: "/new site astro".into(),
        preset_key: "astro".into(),
        template: template.into(),
        site_kind: "docs".into(),
        site_name: "Test Site".into(),
        description: "security fixture".into(),
        accent: "#000000".into(),
        reference: "/tmp".into(),
        reference_label: "tmp".into(),
        site_slug: "test-site".into(),
        preview_base_path: "/api/preview/p/s/test-site".into(),
        preview_url: "/api/preview/p/s/test-site/index.html".into(),
        build_output_dir: build_output_dir.into(),
        project_dir: "sites/test-site".into(),
        pages: Vec::new(),
    }
}

#[test]
fn should_reject_traversal_and_absolute_paths_without_creating_output() {
    let tmp = tempfile::tempdir().unwrap();
    for (value, expected) in [
        ("../escape", BuildOutputDirError::ParentEscape),
        ("/etc/passwd", BuildOutputDirError::Absolute),
        (
            "output/sub/../../../escape",
            BuildOutputDirError::ParentEscape,
        ),
    ] {
        let metadata = site_metadata("astro-site", value);
        assert_eq!(
            validated_build_output_dir(&metadata, tmp.path()),
            Err(expected)
        );
    }
    assert!(!tmp.path().join("escape").exists());
    assert!(!tmp.path().join("output").exists());
}

#[cfg(unix)]
#[test]
fn should_reject_symlink_escape_after_build_without_following_target() {
    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), b"secret").unwrap();
    std::os::unix::fs::symlink(outside.path(), project.path().join("docs")).unwrap();

    let metadata = site_metadata("quarto-lesson", "docs");
    assert_eq!(
        validated_build_output_dir(&metadata, project.path()),
        Err(BuildOutputDirError::OutsideProject)
    );
    assert!(!outside.path().join("created").exists());
}

#[tokio::test]
async fn should_return_400_not_200_on_invalid() {
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use octos_cli::api::testing::{SiteBuildError, preview_build_error_response};

    let response = preview_build_error_response(
        "astro-site",
        SiteBuildError::InvalidMetadata(BuildOutputDirError::ParentEscape),
    );
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("Preview Build Rejected"));
    assert!(!text.contains("/private/") && !text.contains("/Users/"));
}
