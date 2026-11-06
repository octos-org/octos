/// A revoked grant cancels the session's old background work even when its
/// parent already ended. Valid transport loss deliberately does not call this.
async fn abort_revoked_ingress_background_work(
    active_turns: &SharedActiveTurns,
    contracts: &UiProtocolContractStores,
    ledger: &UiProtocolLedger,
    session_id: &SessionKey,
    profile_id: &str,
    generation: u64,
) {
    // Continuation claim + admission uses this same mutex. Stop producers and
    // purge while admission is excluded, then interrupt any already-admitted
    // automatic turn. No session manager or gateway I/O under this lock.
    let active = active_turns.lock().await;
    let Some(background_turns) = contracts
        .background_work
        .revoke_generation(session_id, profile_id, generation)
    else {
        return; // A later request already retired this socket's generation.
    };
    retire_background_continuations(contracts, ledger, session_id, profile_id, background_turns);
    let continuation = active
        .get(session_id)
        .filter(|turn| turn.owner.is_none() && turn.profile_id == profile_id)
        .map(|turn| (turn.state.clone(), turn.interrupt_tx.clone()));
    drop(active);
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

    for (session_id, registered) in turns {
        let turn_id = registered.turn_id.clone();
        let owned = {
            let active = active_turns.lock().await;
            // Reused client IDs do not confer ownership of a newer dispatch.
            if active
                .get(&session_id)
                .is_some_and(|current| !registered.matches(current))
            {
                continue;
            }
            active.get(&session_id).cloned()
        };
        let mut aborted_state: Option<Arc<TokioMutex<TurnState>>> = None;
        let mut aborted_steer: Option<octos_agent::SharedSteerBuffer> = None;
        if let Some(active) = owned {
            aborted_state = Some(active.state.clone());
            aborted_steer = active.steer.clone();
            // UPCR-2026-R4946304A (T-0241): fire the turn token FIRST so
            // in-flight tool workers kill their subprocesses and record
            // cancelled results — work-secret revocation/expiry reaches
            // this path via the socket close, so a revoked client's
            // running shell no longer finishes its writes after the
            // connection is torn down. Bounded grace before the abort.
            active.turn_cancel.cancel();
            if let Some(prompt) = active.dispatched_prompt {
                let _ =
                    tokio::time::timeout(TURN_TOOL_CANCEL_GRACE, prompt.agent_finished.clone())
                        .await;
                prompt.abort_and_wait(Some(&active.abort)).await;
                prompt.persist(&session_id).await;
            } else {
                active.abort.abort();
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
        let mut active = active_turns.lock().await;
        if active
            .get(&session_id)
            .is_some_and(|current| registered.matches(current))
        {
            active.remove(&session_id);
            scopes.evict_session(&session_id);
        } else if !active.contains_key(&session_id) {
            scopes.evict_session(&session_id);
        }
    }
}

include!("ui_protocol_connection_prompts.rs");

/// The caller holds the admission lock. Purge before a replacement generation
/// can create tasks or continuations; delayed old sockets never enter here.
fn retire_background_continuations(
    contracts: &UiProtocolContractStores,
    ledger: &UiProtocolLedger,
    session: &SessionKey,
    profile: &str,
    turns: Vec<TurnId>,
) {
    default_agent_orchestrator().clear_revoked_ingress_continuations(session, profile);
    for turn in turns {
        cancel_connection_turn_prompts(
            session,
            &turn,
            ledger,
            &contracts.approvals,
            &contracts.user_questions,
        );
    }
    contracts.scopes.evict_session(session);
}

fn observe_background_grant(
    state: &AppState,
    contracts: &UiProtocolContractStores,
    ledger: &UiProtocolLedger,
    session: &SessionKey,
    profile: &str,
    expected_hash: Option<&str>,
) -> Result<crate::contracts::background_work::Observation, RpcError> {
    let grants = state.work_secret_store.list().map_err(|error| {
        warn!(%error, "cannot read ingress grants at turn admission");
        RpcError::internal_error("Cannot verify the session's background-work generation")
    })?;
    let stamp = grants
        .into_iter()
        .find(|grant| grant.session_id == session.0)
        .map(|grant| crate::contracts::background_work::GrantStamp {
            active: grant.active(chrono::Utc::now()),
            token_hash: grant.token_hash,
        });
    if expected_hash.is_some_and(|hash| {
        !stamp
            .as_ref()
            .is_some_and(|grant| grant.active && grant.token_hash == hash)
    }) {
        return Err(RpcError::invalid_request(
            "Session ingress grant changed during connection bind",
        ));
    }
    let mut observed = contracts.background_work.observe(session, profile, stamp);
    if let Some(turns) = observed.retired.take() {
        retire_background_continuations(contracts, ledger, session, profile, turns);
        // Preserve the distinction for socket bind, which must interrupt an
        // already-admitted automatic turn after releasing the global lock.
        observed.retired = Some(Vec::new());
    }
    Ok(observed)
}

async fn bind_ingress_generation(
    state: &AppState,
    contracts: &UiProtocolContractStores,
    ledger: &UiProtocolLedger,
    active_turns: &SharedActiveTurns,
    scope: &SessionIngressScope,
    profile: &str,
) -> Result<(u64, crate::contracts::background_work::ScopeLease), (u16, &'static str)> {
    let active = active_turns.lock().await;
    // Validate again under the admission lock. The record read must match this
    // validation, otherwise an upgrade that raced replacement could bind an
    // old bearer to a new generation.
    let grant = state
        .work_secret_store
        .validate(&scope.session_id.0, &scope.token)
        .map_err(|_| (1008, "session_ingress_revoked"))?;
    let observed = observe_background_grant(
        state,
        contracts,
        ledger,
        &scope.session_id,
        profile,
        Some(&grant.token_hash),
    )
    .map_err(|_| (1011, "session_ingress_unavailable"))?;
    let continuation = observed
        .retired
        .and_then(|_| active.get(&scope.session_id))
        .filter(|turn| turn.owner.is_none() && turn.profile_id == profile)
        .map(|turn| (turn.state.clone(), turn.interrupt_tx.clone()));
    drop(active);
    if let Some((state, interrupt_tx)) = continuation {
        let _ = capture_turn_interrupt(state, interrupt_tx, InterruptOrigin::IngressRevoked).await;
    }
    Ok((observed.generation, observed.lifetime))
}
