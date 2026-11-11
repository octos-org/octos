//! Dispatched prompts survive every abnormal turn termination (#2881).
use super::*;

#[tokio::test]
async fn should_commit_disconnected_prompt_before_terminal_without_duplicates() {
    for already_persisted in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionKey("local:disconnect-prompt".into());
        let turn = TurnId::new();
        let text = "Keep the dispatched prompt after disconnect";
        let ledger = Arc::new(UiProtocolLedger::new(16));
        let observer = message_commit_observer(ledger.clone());
        octos_bus::session::set_scoped_message_commit_observer(dir.path(), &observer);
        let sessions = Arc::new(TokioMutex::new(
            octos_bus::SessionManager::open(dir.path()).unwrap(),
        ));
        let context = Arc::new(StdMutex::new(ContextManager::new(
            session.to_string(),
            None,
        )));
        context.lock().unwrap().record_message(&Message::user(text));
        if already_persisted {
            postturn_context_batch::persist_interrupted_turn_prompt(
                &sessions,
                &session,
                &turn.0.to_string(),
                text,
                None,
                dir.path(),
                &context,
            )
            .await;
        }
        // The outer task need not have run its drop guard yet when cleanup
        // starts. Retaining the child handle must cancel AND join the child.
        let child = tokio::spawn(std::future::pending::<()>());
        let child_abort = child.abort_handle();
        let child_finished = async move {
            assert!(child.await.unwrap_err().is_cancelled());
        }
        .boxed()
        .shared();
        let prompt = Arc::new(DispatchedTurnPrompt {
            text: text.into(),
            attribution: None,
            sessions,
            thread_id: turn.0.to_string(),
            data_dir: dir.path().into(),
            context,
            agent_abort: child_abort.clone(),
            agent_finished: child_finished,
        });
        let outer = tokio::spawn(std::future::pending::<()>());
        let mut entry = test_active_turn(turn.clone(), outer.abort_handle());
        entry.dispatched_prompt = Some(prompt);
        let connection = ConnectionTurn {
            finished: std::future::pending().boxed().shared(),
            turn_id: turn.clone(),
            state: entry.state.clone(),
        };
        let active = Arc::new(TokioMutex::new(HashMap::from([(session.clone(), entry)])));
        let connections = Arc::new(TokioMutex::new(HashMap::from([(
            session.clone(),
            connection,
        )])));
        tokio::time::timeout(
            Duration::from_secs(5),
            abort_connection_turns(
                &active,
                &connections,
                &ScopePolicy::default(),
                &ledger,
                &PendingApprovalStore::default(),
                &PendingQuestionStore::default(),
            ),
        )
        .await
        .expect("disconnect must join the cancelled child before committing");
        assert!(child_abort.is_finished());
        assert!(outer.await.unwrap_err().is_cancelled());
        let rows = ledger
            .replay_after(
                &session,
                Some(&UiCursor {
                    stream: session.to_string(),
                    seq: 0,
                }),
            )
            .unwrap();
        let prompt_rows: Vec<_> = rows.iter().enumerate().filter(|(_, row)| {
            matches!(&row.event, UiProtocolLedgerEvent::Notification(UiNotification::EnvelopeV2(envelope))
                if matches!(&envelope.envelope.payload, PayloadV2::UserMessage { text: body, .. } if body == text))
        }).collect();
        assert_eq!(
            prompt_rows.len(),
            1,
            "the prompt must be projected exactly once"
        );
        let terminal_at = rows.iter().position(|row| {
            matches!(&row.event, UiProtocolLedgerEvent::Notification(UiNotification::TurnError(error))
                if error.code == "connection_closed")
        }).unwrap();
        assert!(
            prompt_rows[0].0 < terminal_at,
            "prompt must precede terminal"
        );
        let mut reopened = octos_bus::SessionManager::open(dir.path()).unwrap();
        let history = &reopened.get_or_create(&session).await.messages;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content, text);
        assert_eq!(
            history[0].thread_id.as_deref(),
            Some(turn.0.to_string().as_str())
        );
    }
}
