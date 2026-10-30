use super::*;
use octos_core::ui_protocol::file_mutation_operations;

fn non_git_notice(
    path: &str,
    created: bool,
    previous_content: Option<&str>,
) -> UiFileMutationNotice {
    let mut notice = UiFileMutationNotice::new(path, file_mutation_operations::MODIFY);
    notice.created = Some(created);
    notice.previous_content = previous_content.map(ToOwned::to_owned);
    notice
}

#[test]
fn file_mutation_previous_content_is_not_serialized() {
    let value = serde_json::to_value(non_git_notice("secret.txt", false, Some("old secret")))
        .expect("serialize notice");
    assert!(value.get("created").is_none());
    assert!(value.get("previous_content").is_none());
}

#[test]
fn non_git_create_renders_all_content_as_additions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new.txt");
    std::fs::write(&path, "hello\nworld\n").unwrap();
    let diff =
        materialize_file_mutation_diff(&non_git_notice("new.txt", true, None), Some(dir.path()))
            .expect("non-git create preview");
    assert!(diff.contains("+hello"), "{diff}");
    assert!(diff.contains("+world"), "{diff}");
    assert!(!diff.contains("-hello"), "{diff}");
}

#[test]
fn non_git_modify_uses_authoritative_previous_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("existing.txt");
    std::fs::write(&path, "new\n").unwrap();
    let diff = materialize_file_mutation_diff(
        &non_git_notice("existing.txt", false, Some("old\n")),
        Some(dir.path()),
    )
    .expect("non-git modify preview");
    assert!(diff.contains("-old"), "{diff}");
    assert!(diff.contains("+new"), "{diff}");
}

#[test]
fn non_git_delete_renders_previous_content_as_removals() {
    let dir = tempfile::tempdir().unwrap();
    let diff = materialize_file_mutation_diff(
        &non_git_notice("deleted.txt", false, Some("bye\n")),
        Some(dir.path()),
    )
    .expect("non-git delete preview");
    assert!(diff.contains("-bye"), "{diff}");
    assert!(!diff.contains("+bye"), "{diff}");
}

#[test]
fn non_git_binary_and_oversized_files_do_not_render_huge_hunks() {
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("binary.dat");
    std::fs::write(&binary, [0, 1, 2, 3]).unwrap();
    assert!(
        materialize_file_mutation_diff(&non_git_notice("binary.dat", true, None), Some(dir.path()))
            .is_none()
    );

    let oversized = "x".repeat(MAX_DIFF_PREVIEW_BYTES + 1);
    assert!(
        materialize_file_mutation_diff(
            &non_git_notice("huge.txt", false, Some(&oversized)),
            Some(dir.path()),
        )
        .is_none()
    );
}
