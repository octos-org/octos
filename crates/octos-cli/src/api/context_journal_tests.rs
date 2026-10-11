use super::*;

fn assert_equivalent(actual: &ContextManager, expected: &ContextManager) {
    assert_eq!(
        actual.canonical_ledger_hash(),
        expected.canonical_ledger_hash()
    );
    assert_eq!(actual.transcript_hash(), expected.transcript_hash());
    assert_eq!(actual.source_head_hash(), expected.source_head_hash());
    assert_eq!(actual.compactions(), expected.compactions());
    assert_eq!(actual.cache_epoch(), expected.cache_epoch());
}

#[test]
fn should_append_context_transition_without_rewriting_full_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager =
        ContextManager::from_session_history("journal", None, &[Message::user("start")]);
    manager.reconcile_prompt_cache_epoch("fixture", "model", "stable instructions", &[]);
    let root = persist_context_manager_snapshot(dir.path(), "journal", &manager).unwrap();
    let original = std::fs::read(&root).unwrap();
    manager.record_persisted_message_merging_prompt_equivalent(&Message::assistant("done"), 1);
    persist_context_manager_incremental(dir.path(), "journal", &manager).unwrap();
    assert_eq!(
        std::fs::read(root).unwrap(),
        original,
        "ordinary batch must not rewrite history"
    );
    assert!(
        context_journal::journal_path(dir.path(), "journal")
            .metadata()
            .unwrap()
            .len()
            > 0
    );
    let loaded = load_context_manager_snapshot(dir.path(), "journal")
        .unwrap()
        .unwrap();
    assert_equivalent(&loaded, &manager);
}

#[test]
fn should_retain_stamped_twins_ordered_inserts_and_metadata_without_generation_change() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager =
        ContextManager::from_session_history("journal", None, &[Message::user("start")]);
    manager.record_message(&Message::assistant("foreground"));
    persist_context_manager_snapshot(dir.path(), "journal", &manager).unwrap();
    manager
        .record_persisted_message_merging_prompt_equivalent(&Message::assistant("foreground"), 2);
    let ids = manager
        .record_persisted_message_merging_prompt_equivalent(&Message::assistant("background"), 1);
    persist_context_manager_incremental(dir.path(), "journal", &manager).unwrap();
    let generation = manager.generation();
    manager.mark_source_event_kind(&ids, "background_result");
    assert_eq!(manager.generation(), generation);
    persist_context_manager_incremental(dir.path(), "journal", &manager).unwrap();
    let loaded = load_context_manager_snapshot(dir.path(), "journal")
        .unwrap()
        .unwrap();
    assert_equivalent(&loaded, &manager);
    assert_eq!(loaded.ledger_items(), manager.ledger_items());
}

#[test]
fn should_reject_stale_clone_for_incremental_and_full_checkpoint_writes() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager =
        ContextManager::from_session_history("journal", None, &[Message::user("start")]);
    persist_context_manager_snapshot(dir.path(), "journal", &manager).unwrap();
    let mut stale = manager.clone();
    manager.record_persisted_message_merging_prompt_equivalent(&Message::assistant("newer"), 1);
    persist_context_manager_incremental(dir.path(), "journal", &manager).unwrap();
    stale.record_persisted_message_merging_prompt_equivalent(&Message::assistant("stale"), 1);
    assert!(persist_context_manager_incremental(dir.path(), "journal", &stale).is_err());
    assert!(persist_context_manager_snapshot(dir.path(), "journal", &stale).is_err());
    assert_equivalent(
        &load_context_manager_snapshot(dir.path(), "journal")
            .unwrap()
            .unwrap(),
        &manager,
    );
}

#[test]
fn should_preserve_compaction_cache_epoch_supervisor_facts_and_tool_media() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager = ContextManager::from_session_history(
        "journal",
        None,
        &[
            Message::user("old request ".repeat(100)),
            Message::assistant("old response ".repeat(100)),
            Message::user("recent request"),
        ],
    );
    manager.reconcile_prompt_cache_epoch("fixture", "model", "stable", &[]);
    manager.compact_context(
        "verified old request and result",
        CompactContextPolicy {
            keep_recent_items: 1,
            ..Default::default()
        },
    );
    persist_context_manager_snapshot(dir.path(), "journal", &manager).unwrap();
    manager.record_context_event(ContextEventKind::GoalSnapshot, "goal", "remain active");
    let mut call = Message::assistant("");
    call.tool_calls = Some(vec![ToolCall {
        id: "call_img".into(),
        name: "read_file".into(),
        arguments: json!({"path":"image.png"}),
        metadata: None,
    }]);
    manager.record_persisted_message_merging_prompt_equivalent(&call, 3);
    let mut result =
        Message::tool_with_thread("image", "call_img", octos_core::ThreadId::new("thread"));
    result.media = vec!["data:image/png;base64,aGVsbG8=".into()];
    manager.record_persisted_message_merging_prompt_equivalent(&result, 4);
    manager.observe_effective_provider_route("fixture", "model-2");
    persist_context_manager_incremental(dir.path(), "journal", &manager).unwrap();
    let loaded = load_context_manager_snapshot(dir.path(), "journal")
        .unwrap()
        .unwrap();
    assert_equivalent(&loaded, &manager);
    assert!(loaded.ledger_items().iter().any(|item| matches!(&item.kind,
        TranscriptItemKind::ToolOutput {envelope} if envelope.media == result.media)));
    assert_eq!(
        loaded.snapshot().active_item_ids,
        manager.snapshot().active_item_ids
    );
}

#[test]
fn should_repair_only_an_incomplete_final_transition_before_next_append() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let mut manager =
        ContextManager::from_session_history("journal", None, &[Message::user("start")]);
    persist_context_manager_snapshot(dir.path(), "journal", &manager).unwrap();
    manager.record_persisted_message_merging_prompt_equivalent(&Message::assistant("first"), 1);
    persist_context_manager_incremental(dir.path(), "journal", &manager).unwrap();
    let path = context_journal::journal_path(dir.path(), "journal");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"interrupted")
        .unwrap();
    let mut recovered = load_context_manager_snapshot(dir.path(), "journal")
        .unwrap()
        .unwrap();
    assert_equivalent(&recovered, &manager);
    recovered.record_persisted_message_merging_prompt_equivalent(&Message::assistant("second"), 2);
    persist_context_manager_incremental(dir.path(), "journal", &recovered).unwrap();
    assert_equivalent(
        &load_context_manager_snapshot(dir.path(), "journal")
            .unwrap()
            .unwrap(),
        &recovered,
    );
    let mut bytes = std::fs::read(&path).unwrap();
    let pos = bytes.iter().position(|b| *b == b'\n').unwrap();
    let mut bad: Value = serde_json::from_slice(&bytes[..pos]).unwrap();
    bad["checksum"] = json!("corrupt");
    bytes.splice(..pos, serde_json::to_vec(&bad).unwrap());
    std::fs::write(path, bytes).unwrap();
    assert!(
        load_context_manager_snapshot(dir.path(), "journal").is_err(),
        "complete corrupted record must fail closed"
    );
}

#[test]
fn should_load_promoted_checkpoint_despite_corrupt_or_partial_obsolete_journal() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager =
        ContextManager::from_session_history("journal", None, &[Message::user("start")]);
    persist_context_manager_snapshot(dir.path(), "journal", &manager).unwrap();
    manager.record_persisted_message_merging_prompt_equivalent(&Message::assistant("committed"), 1);
    persist_context_manager_incremental(dir.path(), "journal", &manager).unwrap();
    let old_path = context_journal::journal_path(dir.path(), "journal");
    persist_context_manager_snapshot(dir.path(), "journal", &manager).unwrap();
    assert_ne!(
        old_path,
        context_journal::journal_path(dir.path(), "journal")
    );
    for bytes in [b"{partial".as_slice(), b"corrupt first row\n".as_slice()] {
        // Simulate interruption after atomic root promotion but before cleanup.
        std::fs::write(&old_path, bytes).unwrap();
        assert_equivalent(
            &load_context_manager_snapshot(dir.path(), "journal")
                .unwrap()
                .unwrap(),
            &manager,
        );
    }
}

#[test]
fn should_keep_crash_row_removal_dirty_before_appending_another_transition() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager =
        ContextManager::from_session_history("journal", None, &[Message::user("start")]);
    manager.record_message(&Message::assistant("uncommitted ghost"));
    persist_context_manager_snapshot(dir.path(), "journal", &manager).unwrap();
    let mut recovered = load_context_manager_snapshot(dir.path(), "journal")
        .unwrap()
        .unwrap();
    recovered.record_persisted_message_merging_prompt_equivalent(&Message::assistant("real"), 1);
    persist_context_manager_incremental(dir.path(), "journal", &recovered).unwrap();
    let loaded = load_context_manager_snapshot(dir.path(), "journal")
        .unwrap()
        .unwrap();
    assert_equivalent(&loaded, &recovered);
    assert!(
        !serde_json::to_string(loaded.ledger_items())
            .unwrap()
            .contains("ghost")
    );
}

#[test]
fn should_reject_uncommitted_compaction_and_edited_source_history_after_transition_replay() {
    let dir = tempfile::tempdir().unwrap();
    let history = [Message::user("original"), Message::assistant("old reply")];
    let mut manager = ContextManager::from_session_history("journal", None, &history);
    manager.reconcile_prompt_cache_epoch("fixture", "model", "stable", &[]);
    persist_context_manager_snapshot(dir.path(), "journal", &manager).unwrap();
    manager.record_persisted_message_merging_prompt_equivalent(&Message::assistant("new reply"), 2);
    persist_context_manager_incremental(dir.path(), "journal", &manager).unwrap();
    let changed = [
        Message::user("edited"),
        history[1].clone(),
        Message::assistant("new reply"),
    ];
    let (rebuilt, status) = load_or_rebuild_context_manager(dir.path(), "journal", None, &changed);
    assert_eq!(status, ContextLedgerLoadStatus::Stale);
    assert!(rebuilt.cache_epoch().is_none());
    manager.record_message(&Message::assistant("ghost ".repeat(100)));
    manager.compact_context(
        "contains ghost",
        CompactContextPolicy {
            keep_recent_items: 0,
            ..Default::default()
        },
    );
    persist_context_manager_incremental(dir.path(), "journal", &manager).unwrap();
    assert!(load_context_manager_snapshot(dir.path(), "journal").is_err());
}

#[test]
fn should_migrate_legacy_snapshot_on_first_incremental_commit() {
    let dir = tempfile::tempdir().unwrap();
    let manager = ContextManager::from_session_history("journal", None, &[Message::user("legacy")]);
    let path = context_ledger_path(dir.path(), "journal");
    atomic_write_bytes(&path, &serde_json::to_vec(&manager.snapshot()).unwrap()).unwrap();
    let mut loaded = load_context_manager_snapshot(dir.path(), "journal")
        .unwrap()
        .unwrap();
    loaded.record_persisted_message_merging_prompt_equivalent(&Message::assistant("migrated"), 1);
    persist_context_manager_incremental(dir.path(), "journal", &loaded).unwrap();
    let root: ContextSnapshot = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert!(root.journal_checkpoint_id.is_some());
    assert_equivalent(
        &load_context_manager_snapshot(dir.path(), "journal")
            .unwrap()
            .unwrap(),
        &loaded,
    );
}

#[test]
fn should_not_share_dirty_markers_or_acknowledge_different_scratch_state() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager =
        ContextManager::from_session_history("journal", None, &[Message::user("start")]);
    persist_context_manager_snapshot(dir.path(), "journal", &manager).unwrap();
    let mut scratch = manager.clone();
    scratch.record_persisted_message_merging_prompt_equivalent(&Message::assistant("scratch"), 1);
    assert!(!scratch.acknowledge_identical_context_persistence(&manager));
    manager.record_persisted_message_merging_prompt_equivalent(&Message::assistant("canonical"), 1);
    persist_context_manager_incremental(dir.path(), "journal", &manager).unwrap();
    assert!(persist_context_manager_incremental(dir.path(), "journal", &scratch).is_err());
}

#[test]
fn should_reject_scratch_lineage_rebase_that_drops_newer_facts_or_cache_route() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager =
        ContextManager::from_session_history("journal", None, &[Message::user("start")]);
    manager.reconcile_prompt_cache_epoch("fixture", "model", "stable", &[]);
    persist_context_manager_snapshot(dir.path(), "journal", &manager).unwrap();
    let scratch = manager.clone();
    manager.observe_effective_provider_route("fixture", "model-2");
    persist_context_manager_incremental(dir.path(), "journal", &manager).unwrap();
    assert!(!scratch.rebase_context_persistence_after_merge(&manager));
    let scratch = manager.clone();
    manager.record_context_event(ContextEventKind::GoalSnapshot, "goal", "newer fact");
    persist_context_manager_incremental(dir.path(), "journal", &manager).unwrap();
    assert!(!scratch.rebase_context_persistence_after_merge(&manager));
}

#[test]
fn should_retry_unacknowledged_transition_after_write_failure_without_losing_dirty_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut manager =
        ContextManager::from_session_history("journal", None, &[Message::user("start")]);
    persist_context_manager_snapshot(dir.path(), "journal", &manager).unwrap();
    let path = context_journal::journal_path(dir.path(), "journal");
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    manager.record_persisted_message_merging_prompt_equivalent(&Message::assistant("committed"), 1);
    assert!(persist_context_manager_incremental(dir.path(), "journal", &manager).is_err());
    std::fs::remove_dir(&path).unwrap();
    persist_context_manager_incremental(dir.path(), "journal", &manager).unwrap();
    assert_equivalent(
        &load_context_manager_snapshot(dir.path(), "journal")
            .unwrap()
            .unwrap(),
        &manager,
    );
}
