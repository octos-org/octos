use super::*;
use crate::context_manager::{
    ContextLedgerLoadStatus, context_ledger_path, load_or_rebuild_context_manager,
};

fn request(phase: PromptContextPhase, iteration: u32) -> PromptContextRequest {
    PromptContextRequest {
        phase,
        iteration,
        provider_name: "fixture".into(),
        model_id: "model".into(),
        context_window: 200_000,
    }
}

fn normalized_prompt(manager: &ContextManager) -> Value {
    json!(
        manager
            .for_prompt(&PromptBuildPolicy::default())
            .messages
            .iter()
            .map(|row| {
                json!({"role": row.role, "content": row.content, "tool_calls": row.tool_calls,
            "tool_call_id": row.tool_call_id, "media": row.media})
            })
            .collect::<Vec<_>>()
    )
}

#[test]
fn should_append_inloop_context_without_rewriting_baseline_and_resume_committed_tail() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionKey::new("api", "inloop-incremental");
    let mut history = vec![Message::user("Inspect the deck")];
    let initial = ContextManager::from_session_history(session.to_string(), None, &history);
    let manager = Arc::new(StdMutex::new(initial));
    let bridge = AppUiPromptContextBridge::new(
        session.clone(),
        dir.path().to_owned(),
        manager.clone(),
        false,
    );
    let mut prompt = vec![Message::system("stable runtime")];
    prompt.extend(history.clone());
    bridge
        .prepare_prompt(request(PromptContextPhase::TurnStart, 1), &mut prompt)
        .unwrap();
    let root = context_ledger_path(dir.path(), &session.to_string());
    let baseline = std::fs::read(&root).unwrap();
    for iteration in 2..=5 {
        let id = format!("inspect-{iteration}");
        let mut call = Message::assistant("");
        call.tool_calls = Some(vec![octos_core::ToolCall {
            id: id.clone(),
            name: "view_image".into(),
            arguments: json!({"path": format!("page-{iteration}.png")}),
            metadata: None,
        }]);
        let mut output =
            Message::tool_with_thread("page inspected", id, octos_core::ThreadId::new("deck"));
        output.media = vec![format!("page-{iteration}.png")];
        history.extend([call.clone(), output.clone()]);
        prompt.extend([call, output]);
        bridge
            .prepare_prompt(
                request(PromptContextPhase::Iteration, iteration),
                &mut prompt,
            )
            .unwrap();
        assert!(
            std::fs::read(&root).unwrap() == baseline,
            "ordinary prompt preparation must append a transition, not rewrite the baseline"
        );
    }

    // An interrupted turn must not turn source-less loop rows into committed history.
    let (recovered, status) =
        load_or_rebuild_context_manager(dir.path(), session.to_string(), None, &history[..1]);
    assert_eq!(status, ContextLedgerLoadStatus::Loaded);
    assert_eq!(
        normalized_prompt(&recovered),
        normalized_prompt(&ContextManager::from_session_history(
            session.to_string(),
            None,
            &history[..1]
        ))
    );

    // Once transcript rows are committed, stamp their in-flight twins and resume
    // the exact call/result pairs and media from baseline + transitions.
    let mut batch =
        postturn_context_batch::AppUiContextCommitBatch::new(dir.path(), &manager, &session);
    for (seq, row) in history.iter().enumerate().skip(1) {
        batch.record(row, seq);
    }
    batch.finish().unwrap();
    let (loaded, status) =
        load_or_rebuild_context_manager(dir.path(), session.to_string(), None, &history);
    assert_eq!(status, ContextLedgerLoadStatus::Loaded);
    assert_eq!(
        normalized_prompt(&loaded),
        normalized_prompt(&manager.lock().unwrap())
    );
    assert!(std::fs::read(root).unwrap() == baseline);
}

#[test]
fn should_checkpoint_inloop_compaction_then_return_to_incremental_writes() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionKey::new("api", "inloop-compaction-checkpoint");
    let mut history = Vec::new();
    for i in 0..20 {
        history.push(Message::user(format!("old task {i}: {}", "x".repeat(400))));
        history.push(Message::assistant(format!(
            "old answer {i}: {}",
            "y".repeat(400)
        )));
    }
    history.push(Message::user("Finish the current task"));
    let mut initial = ContextManager::from_session_history(session.to_string(), None, &history);
    initial.reconcile_prompt_cache_epoch("fixture", "model", "stable runtime", &[]);
    let manager = Arc::new(StdMutex::new(initial));
    let bridge = AppUiPromptContextBridge::new(
        session.clone(),
        dir.path().to_owned(),
        manager.clone(),
        false,
    );
    let mut prompt = vec![Message::system("stable runtime")];
    prompt.extend(history.clone());
    bridge
        .prepare_prompt(request(PromptContextPhase::TurnStart, 1), &mut prompt)
        .unwrap();
    let root = context_ledger_path(dir.path(), &session.to_string());
    let baseline = std::fs::read(&root).unwrap();
    let mut compact = request(PromptContextPhase::Iteration, 2);
    compact.context_window = 300;
    let report = bridge.prepare_prompt(compact, &mut prompt).unwrap();
    assert!(report.compaction_performed);
    let compacted = std::fs::read(&root).unwrap();
    assert!(
        compacted != baseline,
        "compaction must install a new baseline"
    );
    let (loaded, status) =
        load_or_rebuild_context_manager(dir.path(), session.to_string(), None, &history);
    assert_eq!(status, ContextLedgerLoadStatus::Loaded);
    assert_eq!(loaded.compactions(), manager.lock().unwrap().compactions());
    assert_eq!(loaded.cache_epoch(), manager.lock().unwrap().cache_epoch());
    assert_eq!(
        normalized_prompt(&loaded),
        normalized_prompt(&manager.lock().unwrap())
    );

    bridge
        .prepare_prompt(request(PromptContextPhase::Iteration, 3), &mut prompt)
        .unwrap();
    assert!(
        std::fs::read(root).unwrap() == compacted,
        "the iteration after compaction must return to incremental persistence"
    );
    let (loaded, status) =
        load_or_rebuild_context_manager(dir.path(), session.to_string(), None, &history);
    assert_eq!(status, ContextLedgerLoadStatus::Loaded);
    assert_eq!(
        normalized_prompt(&loaded),
        normalized_prompt(&manager.lock().unwrap())
    );
}

#[test]
fn should_retry_failed_inloop_append_without_losing_pending_rows() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionKey::new("api", "inloop-append-retry");
    let mut history = vec![Message::user("finish deck")];
    let manager = Arc::new(StdMutex::new(ContextManager::from_session_history(
        session.to_string(),
        None,
        &history,
    )));
    let bridge = AppUiPromptContextBridge::new(
        session.clone(),
        dir.path().to_owned(),
        manager.clone(),
        false,
    );
    let mut prompt = history.clone();
    bridge
        .prepare_prompt(request(PromptContextPhase::TurnStart, 1), &mut prompt)
        .unwrap();
    let root = context_ledger_path(dir.path(), &session.to_string());
    let baseline = std::fs::read(&root).unwrap();
    let journal = std::fs::read_dir(root.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.to_string_lossy().ends_with(".transitions.jsonl"))
        .unwrap();
    std::fs::remove_file(&journal).unwrap();
    std::fs::create_dir(&journal).unwrap();
    let first = Message::assistant("first pending result");
    history.push(first.clone());
    prompt.push(first);
    // Rebuildable-context persistence is best effort; failure must not clear
    // the dirty cursor or abort the model turn.
    bridge
        .prepare_prompt(request(PromptContextPhase::Iteration, 2), &mut prompt)
        .unwrap();
    assert!(std::fs::read(&root).unwrap() == baseline);
    std::fs::remove_dir(&journal).unwrap();
    std::fs::write(&journal, []).unwrap();
    let second = Message::assistant("second pending result");
    history.push(second.clone());
    prompt.push(second);
    bridge
        .prepare_prompt(request(PromptContextPhase::Iteration, 3), &mut prompt)
        .unwrap();
    assert!(std::fs::metadata(&journal).unwrap().len() > 0);
    let mut batch =
        postturn_context_batch::AppUiContextCommitBatch::new(dir.path(), &manager, &session);
    for (seq, row) in history.iter().enumerate().skip(1) {
        batch.record(row, seq);
    }
    batch.finish().unwrap();
    let (loaded, status) =
        load_or_rebuild_context_manager(dir.path(), session.to_string(), None, &history);
    assert_eq!(status, ContextLedgerLoadStatus::Loaded);
    assert_eq!(
        normalized_prompt(&loaded),
        normalized_prompt(&manager.lock().unwrap())
    );
    assert!(
        normalized_prompt(&loaded)
            .to_string()
            .contains("first pending result")
    );
    assert!(std::fs::read(root).unwrap() == baseline);
}
