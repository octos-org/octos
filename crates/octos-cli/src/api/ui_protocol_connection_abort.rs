/// A revoked grant cancels the session's old background work even when its
/// parent already ended. Valid transport loss deliberately does not call this.
async fn abort_revoked_ingress_background_work(
    active_turns: &SharedActiveTurns,
    contracts: &UiProtocolContractStores,
    ledger: &UiProtocolLedger,
    session_id: &SessionKey,
    profile_id: &str,
) {
    // Continuation claim + admission uses this same mutex. Stop producers and
    // purge while admission is excluded, then interrupt any already-admitted
    // automatic turn. No session manager or gateway I/O under this lock.
    let active = active_turns.lock().await;
    let background_turns = contracts.background_work.revoke(session_id, profile_id);
    default_agent_orchestrator().clear_revoked_ingress_continuations(session_id, profile_id);
    let continuation = active
        .get(session_id)
        .filter(|turn| turn.owner.is_none() && turn.profile_id == profile_id)
        .map(|turn| (turn.state.clone(), turn.interrupt_tx.clone()));
    drop(active);
    for turn_id in background_turns {
        cancel_connection_turn_prompts(
            session_id,
            &turn_id,
            ledger,
            &contracts.approvals,
            &contracts.user_questions,
        );
    }
    if let Some((state, interrupt_tx)) = continuation {
        let _ = capture_turn_interrupt(state, interrupt_tx, InterruptOrigin::IngressRevoked).await;
    }
}

async fn abort_connection_turns(
    active_turns: &SharedActiveTurns,
    connection_turns: &SharedConnectionTurns,
    scopes: &ScopePolicy,
    ledger: &UiProtocolLedger,
    approvals: &PendingApprovalStore,
    user_questions: &PendingQuestionStore,
) {
    let turns = std::mem::take(&mut *connection_turns.lock().await);
    if turns.is_empty() {
        return;
    }

    let mut active = active_turns.lock().await;
    for (session_id, registered) in turns {
        let turn_id = registered.turn_id.clone();
        // Reused client IDs do not confer ownership of a newer dispatch.
        if active
            .get(&session_id)
            .is_some_and(|current| !registered.matches(current))
        {
            continue;
        }
        let mut aborted_state: Option<Arc<TokioMutex<TurnState>>> = None;
        let mut aborted_steer: Option<octos_agent::SharedSteerBuffer> = None;
        let should_abort = active
            .get(&session_id)
            .is_some_and(|active| registered.matches(active));
        if should_abort {
            if let Some(active) = active.remove(&session_id) {
                aborted_state = Some(active.state.clone());
                aborted_steer = active.steer.clone();
                if let Some(prompt) = active.dispatched_prompt {
                    prompt.abort_and_wait(Some(&active.abort)).await;
                    prompt.persist(&session_id).await;
                } else {
                    active.abort.abort();
                }
            }
        }
        release_peer_build_cache_slot(
            None,
            &session_id,
            &turn_id,
            &registered.state,
            crate::build_cache::pool::SlotOutcome::Cancelled,
        );
        // #920.1: append a durable terminal so reconnect replay sees the
        // turn end, using the same single-fire transition as the normal
        // lifecycle to avoid racing natural completion or interruption.
        if let Some(state) = aborted_state {
            // task-return-unconsumed-steer-inputs: same terminal gate as the
            // live path — state Terminal → `turn/steer_dropped` (ledger-only:
            // the connection is gone, the reconnecting client replays it) →
            // terminal. Without this the aborted turn task's safety-net drain
            // would land AFTER this terminal and a same-process reconnect
            // would lose the steer.
            if let Some(transition) = transition_to_terminal_settling_steers(
                state.as_ref(),
                TerminalReason::Interrupted,
                aborted_steer.as_ref(),
                SteerReturnSink::LedgerOnly { ledger },
                &session_id,
                &turn_id,
            )
            .await
            {
                let terminal_append = ledger.try_append_notification(UiNotification::TurnError(
                    TurnErrorEvent {
                        session_id: session_id.clone(),
                        topic: None,
                        turn_id: turn_id.clone(),
                        code: "connection_closed".to_owned(),
                        message: "Connection-bound turn stopped when its transport closed. Reconnect, review the retained prompt, and resend to start a new turn.".to_owned(),
                        token_usage: None,
                        partial_result: None,
                    },
                ));
                if let Err(error) = &terminal_append {
                    warn_ledger_failure("connection-closed terminal", error);
                }
                // OLP L1 (slice 5): turn_error event, best-effort. The
                // ledger's configured data dir is the same root serve
                // writes events.jsonl to; None (RAM-only) drops the event.
                if let Some(data_dir) = ledger.config_data_dir() {
                    crate::obs_events::append_obs_event(
                        &data_dir,
                        &crate::obs_events::ObsEvent::new(
                            "turn_error",
                            "connection closed before turn completed",
                        )
                        .session(Some(session_id.0.as_str())),
                    );
                }
                if terminal_append.is_ok()
                    && let Some(ack) = transition.ack
                {
                    let _ = ack.send(());
                }
            }
        }
        cancel_connection_turn_prompts(&session_id, &turn_id, ledger, approvals, user_questions);
        // FIX-06: connection close is the de-facto "session close" hook in
        // v1alpha1 — drop every recorded scope for this session so it cannot
        // outlive the WebSocket. Per M9-FIX-06 § "Out of scope", an explicit
        // `session/close` wire event would be a cleaner trigger; until then
        // this best-effort hook is the canonical place.
        scopes.evict_session(&session_id);
    }
}

include!("ui_protocol_connection_prompts.rs");
