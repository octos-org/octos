use super::*;
use octos_core::ui_protocol::{DiffPreviewGetParams, file_mutation_operations};

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

async fn assert_non_git_binary_baseline_is_not_previewed(bytes: &[u8], delete: bool) {
    use octos_agent::tools::{Tool, apply_patch::ApplyPatchTool, write_file::WriteFileTool};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("document.bin");
    std::fs::write(&path, bytes).unwrap();
    if bytes.starts_with(b"%PDF-") {
        assert!(
            octos_agent::tools::read_no_follow(&path)
                .await
                .unwrap()
                .contains("PDF_BASELINE_SECRET"),
            "fixture must reproduce the transforming reader's extracted text"
        );
    }
    let result = if delete {
        ApplyPatchTool::new(dir.path())
            .execute(&json!({"patch": "*** Begin Patch\n*** Delete File: document.bin\n*** End Patch\n"}))
            .await
            .unwrap()
    } else {
        WriteFileTool::new(dir.path())
            .execute(&json!({"path": "document.bin", "content": "replacement\n"}))
            .await
            .unwrap()
    };
    assert!(result.success, "{}", result.output);
    assert!(!dir.path().join(".git").exists());
    assert_eq!(path.exists(), !delete);
    let metadata = result.structured_metadata.unwrap();
    let notice = non_git_notice(
        "document.bin",
        metadata["file_created"].as_bool().unwrap(),
        metadata["previous_content"].as_str(),
    );
    let preview = materialize_file_mutation_diff(&notice, Some(dir.path()));
    assert!(
        preview.is_none(),
        "binary baseline produced a text diff: {preview:?}"
    );
    assert!(metadata["previous_content"].is_null());
}

#[tokio::test]
async fn non_git_pdf_delete_does_not_preview_extracted_text() {
    assert_non_git_binary_baseline_is_not_previewed(
        include_bytes!("test_fixtures/non_git_text.pdf"),
        true,
    )
    .await;
}

#[tokio::test]
async fn non_git_pdf_overwrite_does_not_preview_extracted_text() {
    assert_non_git_binary_baseline_is_not_previewed(
        include_bytes!("test_fixtures/non_git_text.pdf"),
        false,
    )
    .await;
}

#[tokio::test]
async fn non_git_binary_delete_does_not_preview_previous_bytes() {
    assert_non_git_binary_baseline_is_not_previewed(b"\0old binary\xff", true).await;
}

#[tokio::test]
async fn non_git_binary_overwrite_does_not_preview_previous_bytes() {
    assert_non_git_binary_baseline_is_not_previewed(b"\0old binary\xff", false).await;
}

#[tokio::test]
async fn non_git_pdf_edit_does_not_preview_extracted_text() {
    use octos_agent::tools::{Tool, edit_file::EditFileTool, write_grant::WritePathGrant};

    for fenced in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("document.bin"),
            include_bytes!("test_fixtures/non_git_text.pdf"),
        )
        .unwrap();
        let mut tool = EditFileTool::new(dir.path());
        if fenced {
            tool = tool
                .with_write_grant(WritePathGrant::new(&["document.bin".into()], false).unwrap());
        }
        let result = tool.execute(&json!({
            "path": "document.bin", "old_string": "PDF_BASELINE_SECRET", "new_string": "replacement"
        })).await.unwrap();
        assert!(result.success, "{}", result.output);
        let metadata = result.structured_metadata.unwrap();
        let notice = non_git_notice("document.bin", false, metadata["previous_content"].as_str());
        let preview = materialize_file_mutation_diff(&notice, Some(dir.path()));
        assert!(
            preview.is_none(),
            "fenced={fenced}: PDF baseline rendered: {preview:?}"
        );
        assert!(metadata["previous_content"].is_null());
    }
}

#[test]
fn file_mutation_preview_releases_baseline_before_live_and_cold_replay() {
    for mode in ["materialized", "explicit", "unavailable", "git", "create"] {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("file.txt");
        if mode == "git" {
            std::fs::write(&path, "old\n").unwrap();
            for args in [vec!["init"], vec!["add", "file.txt"]] {
                assert!(
                    Command::new("git")
                        .arg("-C")
                        .arg(workspace.path())
                        .args(args)
                        .output()
                        .unwrap()
                        .status
                        .success()
                );
            }
        }
        std::fs::write(&path, if mode == "unavailable" { "\0" } else { "new\n" }).unwrap();
        let session_id = SessionKey(format!("local:baseline-{mode}"));
        let context = ProgressMappingContext::new(session_id.clone(), TurnId::new());
        let contracts = UiProtocolContractStores::default();
        let mut event = json!({
            "type": "file_modified", "path": "file.txt", "created": mode == "create",
            "previous_content": "old\n", "tool_call_id": "writer",
        });
        if mode == "explicit" {
            event["diff"] = json!(
                "diff --git a/file.txt b/file.txt\n--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n"
            );
        }
        let mut mapping = map_progress_json(&context, &event);
        apply_progress_contract_side_effects(
            &contracts,
            &context,
            Some(workspace.path()),
            &event,
            &mut mapping,
        );
        let status = mapping.status.unwrap();
        let notice = status.event.metadata.file_mutation.as_ref().unwrap();
        let preview = contracts
            .diff_previews(None)
            .get(DiffPreviewGetParams {
                session_id: session_id.clone(),
                preview_id: notice.preview_id.clone().unwrap(),
            })
            .unwrap();
        if mode != "unavailable" {
            assert!(!preview.preview.files[0].hunks.is_empty(), "{mode}");
        }

        let data = tempfile::tempdir().unwrap();
        let config = LedgerConfig::durable(data.path().to_path_buf());
        let ledger = UiProtocolLedger::with_config(config.clone());
        ledger.append_progress(status.event);
        let (live, _) = ledger.snapshot_with_cursor(&session_id, None).unwrap();
        drop(ledger);
        let recovered = UiProtocolLedger::recover(config).ledger;
        let (cold, _) = recovered.snapshot_with_cursor(&session_id, None).unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(cold.len(), 1);
        assert_eq!(
            live[0].event, cold[0].event,
            "{mode}: in-memory and disk replay differ"
        );
        let UiProtocolLedgerEvent::Progress(progress) = &live[0].event else {
            panic!("progress expected")
        };
        let notice = progress.metadata.file_mutation.as_ref().unwrap();
        assert_eq!(notice.previous_content, None, "{mode}");
        assert_eq!(notice.created, None, "{mode}");
    }
}

#[test]
fn skill_action_result_does_not_expose_file_mutation_baselines() {
    let workspace = tempfile::tempdir().unwrap();
    for (metadata, expected) in [
        (
            Some(
                json!({"file_created": false, "previous_content": "private baseline", "source": {"id": "src_report"}}),
            ),
            json!({"source": {"id": "src_report"}}),
        ),
        (
            Some(json!({"file_created": true, "previous_content": null})),
            Value::Null,
        ),
        (None, Value::Null),
    ] {
        let result = octos_agent::ToolResult {
            success: true,
            structured_metadata: metadata,
            ..Default::default()
        };
        let value = tool_result_to_action_value(result, workspace.path());
        assert_eq!(value["structured_metadata"], expected);
    }
}

#[test]
fn non_git_preview_rejects_absolute_and_parent_escapes() {
    let parent = tempfile::tempdir().unwrap();
    let workspace = parent.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let outside = parent.path().join("private.txt");
    std::fs::write(&outside, "outside secret\n").unwrap();
    for path in [
        outside.to_str().unwrap(),
        "../private.txt",
        "../deleted.txt",
    ] {
        let notice = non_git_notice(path, true, Some("previous secret\n"));
        assert!(
            materialize_file_mutation_diff(&notice, Some(&workspace)).is_none(),
            "{path}"
        );
    }
    assert!(
        materialize_file_mutation_diff(
            &non_git_notice(outside.to_str().unwrap(), true, None),
            None,
        )
        .is_none(),
        "without a session root, an absolute path cannot grant access outside cwd"
    );
}

#[cfg(unix)]
#[test]
fn non_git_preview_rejects_symlink_escapes_and_accepts_symlinked_workspace() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    let outside = dir.path().join("outside");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("private.txt"), "outside secret\n").unwrap();
    symlink(&outside, workspace.join("escape")).unwrap();
    symlink(outside.join("private.txt"), workspace.join("file-link")).unwrap();
    symlink(outside.join("missing.txt"), workspace.join("dangling")).unwrap();
    for path in [
        "escape/private.txt",
        "escape/deleted.txt",
        "file-link",
        "dangling",
    ] {
        assert!(
            materialize_file_mutation_diff(
                &non_git_notice(path, true, Some("old\n")),
                Some(&workspace),
            )
            .is_none(),
            "{path}"
        );
    }
    let alias = dir.path().join("alias");
    symlink(&workspace, &alias).unwrap();
    std::fs::write(workspace.join("new.txt"), "safe\n").unwrap();
    assert!(
        materialize_file_mutation_diff(&non_git_notice("new.txt", true, None), Some(&alias),)
            .unwrap()
            .contains("+safe")
    );
    assert!(
        materialize_file_mutation_diff(
            &non_git_notice("deleted.txt", false, Some("old\n")),
            Some(&alias),
        )
        .unwrap()
        .contains("-old")
    );
}

#[test]
fn non_git_unrenderable_current_content_is_not_a_deletion() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file.txt");
    for bytes in [
        vec![0, 1, 2],
        vec![255],
        vec![b'x'; MAX_DIFF_PREVIEW_BYTES + 1],
    ] {
        std::fs::write(&path, bytes).unwrap();
        assert!(
            materialize_file_mutation_diff(
                &non_git_notice("file.txt", false, Some("old\n")),
                Some(dir.path()),
            )
            .is_none(),
            "unreadable current text must not render a false deletion"
        );
    }
}
