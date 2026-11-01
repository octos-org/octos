#[tokio::test(flavor = "current_thread")]
async fn session_resume_rejects_future_context_after_interrupted_rollback_checkpoint() {
    let session_id = SessionKey("local:rollback-checkpoint-window".into());
    let (state, _tmp) = prg_state_with_persisted_turns(&session_id, 3).await;
    let data_dir = {
        let mut sessions = state.sessions.as_ref().unwrap().lock().await;
        let data_dir = sessions.data_dir();
        let history = sessions.get_or_create(&session_id).await.messages.clone();
        let mut context =
            ContextManager::from_session_history(session_id.to_string(), None, &history);
        context.reconcile_prompt_cache_epoch("fixture", "future-model", "stable", &[]);
        context.compact_context(
            "FUTURE-SUMMARY",
            crate::context_manager::CompactContextPolicy {
                keep_recent_items: 2,
                ..Default::default()
            },
        );
        persist_context_manager_snapshot(&data_dir, &session_id.to_string(), &context).unwrap();
        context.record_context_event(
            crate::context_manager::ContextEventKind::GoalSnapshot,
            "goal",
            "FUTURE-FACT",
        );
        crate::context_manager::persist_context_manager_incremental(
            &data_dir,
            &session_id.to_string(),
            &context,
        )
        .unwrap();
        // The actual rollback API commits its marker before the handler resets
        // context. Simulate a crash at precisely that boundary.
        sessions
            .rollback_last_n_user_turns(&session_id, 1)
            .await
            .unwrap();
        data_dir
    };
    let mut restarted_sessions = octos_bus::SessionManager::open(&data_dir).unwrap();
    let history = restarted_sessions
        .get_or_create(&session_id)
        .await
        .messages
        .clone();
    assert_eq!(history.len(), 4);
    // Exercise normal session-open context hydration, not a manual reset.
    appui_context_open_snapshot(&data_dir, &session_id, &history, None);
    let restored =
        crate::context_manager::load_context_manager_snapshot(&data_dir, &session_id.to_string())
            .unwrap()
            .unwrap();
    assert!(restored.compactions().is_empty());
    assert!(restored.cache_epoch().is_none());
    let state_json = serde_json::to_string(&restored.snapshot()).unwrap();
    for dropped in [
        "FUTURE-SUMMARY",
        "FUTURE-FACT",
        "turn 3",
        "reply 3",
        "future-model",
    ] {
        assert!(
            !state_json.contains(dropped),
            "dropped context resurrected: {dropped}"
        );
    }
    let (_, status) =
        load_or_rebuild_context_manager(&data_dir, session_id.to_string(), None, &history);
    assert_eq!(
        status,
        crate::context_manager::ContextLedgerLoadStatus::Loaded
    );
}
