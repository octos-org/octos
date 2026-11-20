/// A revoked grant cancels the session's old background work even when its
/// parent already ended. Valid transport loss deliberately does not call this.
async fn abort_revoked_ingress_background_work(
    active_turns: &SharedActiveTurns,
    contracts: &UiProtocolContractStores,
    ledger: &UiProtocolLedger,
    session_id: &SessionKey,
    profile_id: &str,
    turn_profile: &str,
    generation: u64,
) {
    let active = active_turns.lock().await;
    let Some(background_turns) = contracts
        .background_work
        .revoke_generation(session_id, profile_id, generation)
    else {
        return; // A later request already retired this socket's generation.
    };
    finish_revoked_ingress_background_work(
        active,
        contracts,
        ledger,
        session_id,
        profile_id,
        turn_profile,
        background_turns,
    )
    .await;
}

async fn finish_revoked_ingress_background_work(
    active: tokio::sync::MutexGuard<'_, HashMap<SessionKey, ActiveTurn>>,
    contracts: &UiProtocolContractStores,
    ledger: &UiProtocolLedger,
    session_id: &SessionKey,
    profile_id: &str,
    turn_profile: &str,
    background_turns: Vec<TurnId>,
) {
    // Continuation claim + admission uses this same mutex. Stop producers and
    // purge while admission is excluded, then interrupt any already-admitted
    // automatic turn. No session manager or gateway I/O under this lock.
    retire_background_continuations(contracts, ledger, session_id, profile_id, background_turns);
    let continuation = active
        .get(session_id)
        .filter(|turn| turn.owner.is_none() && turn.profile_id == turn_profile)
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
            // UPCR-2026-R4946304A (T-0241): CLAIM the interrupt BEFORE the
            // token and grace below. Without this capture, the agent loop's
            // graceful wind-down (the T-0241 early-return) records
            // `Terminal(Completed)` DURING the grace and the `Interrupted`
            // transition at the bottom of this function becomes a no-op —
            // a revoked/aborted turn read to reconnecting clients as a
            // normal completion (regressed #2979). The state mutex makes
            // this single-fire: an already-terminal or already-interrupting
            // turn just falls through to the same code as before.
            let _ = capture_turn_interrupt(
                active.state.clone(),
                active.interrupt_tx.clone(),
                InterruptOrigin::ConnectionClosed,
            )
            .await;
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

/// Serialize snapshot read and generation observation per session/profile.
/// Different sessions keep running during slow storage, while an older read
/// cannot complete after a replacement and overwrite its newer grant stamp.
/// Lock order: grant observation → active_turns → BackgroundWorkStore.
async fn lock_background_grant_observation(
    session: &SessionKey,
    profile: &str,
) -> tokio::sync::OwnedMutexGuard<()> {
    type GrantLocks = std::sync::Mutex<
        HashMap<(String, String), std::sync::Weak<tokio::sync::Mutex<()>>>,
    >;
    static LOCKS: std::sync::OnceLock<GrantLocks> = std::sync::OnceLock::new();
    let key = (session.0.clone(), profile.to_owned());
    let lock = {
        let mut locks = LOCKS
            .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
            .lock()
            .unwrap();
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&key).and_then(std::sync::Weak::upgrade) {
            lock
        } else {
            let lock = Arc::new(tokio::sync::Mutex::new(()));
            locks.insert(key, Arc::downgrade(&lock));
            lock
        }
    };
    lock.lock_owned().await
}

async fn load_background_grants(
    state: &AppState,
) -> eyre::Result<octos_agent::bridge::work_secret::WorkSecretGrantSnapshot> {
    state.work_secret_store.list_for_admission_async().await
}

fn observe_background_grant_read(
    read: &eyre::Result<octos_agent::bridge::work_secret::WorkSecretGrantSnapshot>,
    contracts: &UiProtocolContractStores,
    ledger: &UiProtocolLedger,
    session: &SessionKey,
    profile: &str,
    expected_hash: Option<&str>,
    fail_closed_on_error: bool,
) -> Result<crate::contracts::background_work::Observation, RpcError> {
    let grants = match read {
        Ok(snapshot) => snapshot.records().to_vec(),
        Err(error) if !fail_closed_on_error => {
            // Ordinary browser / non-ingress turn (T-0309): a corrupt or
            // unreadable grants file must not strip existing browser
            // capabilities, and must NOT be read as a revocation (which would
            // retire and cancel legitimate background work). Degrade to a
            // read-only observation of the current generation; nothing is
            // cached, so the next admission after the file is repaired
            // observes normally. The error detail names no credential.
            warn!(%error,
                "cannot read ingress grants; admitting non-ingress turn without generation change");
            return Ok(contracts.background_work.peek(session, profile));
        }
        Err(error) => {
            // Ingress bind (a secret was presented) stays fail-closed.
            warn!(%error, "cannot read ingress grants at turn admission");
            return Err(RpcError::internal_error(
                "Cannot verify the session's background-work generation",
            ));
        }
    };
    observe_background_grant_snapshot(
        contracts,
        ledger,
        session,
        profile,
        grants,
        expected_hash,
        chrono::Utc::now(),
    )
}

fn observe_background_grant_snapshot(
    contracts: &UiProtocolContractStores,
    ledger: &UiProtocolLedger,
    session: &SessionKey,
    profile: &str,
    grants: Vec<octos_agent::bridge::work_secret::WorkSecretGrantRecord>,
    expected_hash: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<crate::contracts::background_work::Observation, RpcError> {
    let stamp = grants
        .into_iter()
        .find(|grant| grant.session_id == session.0)
        .map(|grant| crate::contracts::background_work::GrantStamp {
            active: grant.active(now),
            token_hash: grant.token_hash,
            created_at: grant.created_at,
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

/// The background-work scope an ingress socket owns, resolved in the order
/// turn admission uses, so a revocation reaches the profile the work runs under.
fn ingress_background_profile(
    state: &AppState,
    session_id: &SessionKey,
    connection_profile_id: Option<&str>,
) -> String {
    let active = session_id
        .profile_id()
        .or(connection_profile_id)
        .map(ToOwned::to_owned)
        .or_else(|| session_open_profiles().lookup(session_id));
    resolve_session_profile_runtime(state, active.as_deref())
        .map(|runtime| runtime.profile_id.clone())
        .or(active)
        .unwrap_or_else(|| MAIN_PROFILE_ID.to_owned())
}

async fn reject_ingress_bind(
    active: tokio::sync::MutexGuard<'_, HashMap<SessionKey, ActiveTurn>>,
    contracts: &UiProtocolContractStores,
    ledger: &UiProtocolLedger,
    scope: &SessionIngressScope,
    profile: &str,
    turn_profile: &str,
) -> (u16, &'static str) {
    let background_turns = contracts.background_work.revoke_active_grant(
        &scope.session_id,
        profile,
        &octos_agent::bridge::work_secret::hash_token(&scope.token),
        scope.grant_created_at,
    );
    if let Some(background_turns) = background_turns {
        finish_revoked_ingress_background_work(
            active,
            contracts,
            ledger,
            &scope.session_id,
            profile,
            turn_profile,
            background_turns,
        )
        .await;
    } else {
        drop(active);
    }
    (1008, "session_ingress_revoked")
}

#[cfg(test)]
fn test_ingress_bind_read_gates() -> &'static std::sync::Mutex<
    HashMap<SessionKey, Arc<tokio::sync::Semaphore>>,
> {
    static GATES: std::sync::OnceLock<
        std::sync::Mutex<HashMap<SessionKey, Arc<tokio::sync::Semaphore>>>,
    > = std::sync::OnceLock::new();
    GATES.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Delay one test socket immediately before its bind-time grant read. This
/// replaces the old active-turn lock barrier, which cannot order a read that
/// now deliberately happens before that lock.
#[cfg(test)]
fn install_ingress_bind_read_gate(session: &SessionKey) -> Arc<tokio::sync::Semaphore> {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    test_ingress_bind_read_gates()
        .lock()
        .unwrap()
        .insert(session.clone(), gate.clone());
    gate
}

#[cfg(test)]
async fn await_ingress_bind_read_gate(session: &SessionKey) {
    let gate = test_ingress_bind_read_gates().lock().unwrap().remove(session);
    if let Some(gate) = gate {
        let _permit = gate.acquire().await.expect("test bind gate remains open");
    }
}

#[cfg(not(test))]
async fn await_ingress_bind_read_gate(_session: &SessionKey) {}

#[cfg(test)]
type IngressAdmissionGate = (Arc<tokio::sync::Semaphore>, Arc<tokio::sync::Semaphore>);

#[cfg(test)]
fn test_ingress_bind_admission_gates() -> &'static std::sync::Mutex<
    HashMap<SessionKey, IngressAdmissionGate>,
> {
    static GATES: std::sync::OnceLock<
        std::sync::Mutex<HashMap<SessionKey, IngressAdmissionGate>>,
    > = std::sync::OnceLock::new();
    GATES.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

#[cfg(test)]
fn install_ingress_bind_admission_gate(session: &SessionKey) -> IngressAdmissionGate {
    let gate = (
        Arc::new(tokio::sync::Semaphore::new(0)),
        Arc::new(tokio::sync::Semaphore::new(0)),
    );
    test_ingress_bind_admission_gates()
        .lock()
        .unwrap()
        .insert(session.clone(), gate.clone());
    gate
}

#[cfg(test)]
async fn await_ingress_bind_admission_gate(session: &SessionKey) {
    let gate = test_ingress_bind_admission_gates()
        .lock()
        .unwrap()
        .remove(session);
    if let Some((entered, release)) = gate {
        entered.add_permits(1);
        let _permit = release.acquire().await.expect("admission gate remains open");
    }
}

#[cfg(not(test))]
async fn await_ingress_bind_admission_gate(_session: &SessionKey) {}

async fn bind_ingress_generation(
    state: &AppState,
    contracts: &UiProtocolContractStores,
    ledger: &UiProtocolLedger,
    active_turns: &SharedActiveTurns,
    scope: &SessionIngressScope,
    profile: &str,
    turn_profile: &str,
) -> Result<(u64, crate::contracts::background_work::ScopeLease), (u16, &'static str)> {
    // Complete the blocking read before taking the global admission lock. The
    // same snapshot and timestamp still drive validation and observation.
    await_ingress_bind_read_gate(&scope.session_id).await;
    let _grant_guard = lock_background_grant_observation(&scope.session_id, profile).await;
    let grant_snapshot = state
        .work_secret_store
        .list_for_admission_async()
        .await
        .map_err(|error| {
            warn!(%error, "cannot read ingress grants during connection bind");
            (1011, "session_ingress_unavailable")
        })?;
    await_ingress_bind_admission_gate(&scope.session_id).await;
    let active = active_turns.lock().await;
    let now = chrono::Utc::now();
    let grant = match octos_agent::bridge::work_secret::WorkSecretGrantStore::validate_records_at(
        grant_snapshot.records(),
        &scope.session_id.0,
        &scope.token,
        now,
    ) {
        Ok(grant) if grant.created_at == scope.grant_created_at => grant,
        Ok(_) | Err(_) => {
            return Err(reject_ingress_bind(active, contracts, ledger, scope, profile, turn_profile).await);
        }
    };
    let observed = match observe_background_grant_snapshot(
        contracts,
        ledger,
        &scope.session_id,
        profile,
        grant_snapshot.records().to_vec(),
        Some(&grant.token_hash),
        now,
    ) {
        Ok(observed) => observed,
        Err(_) => {
            return Err(reject_ingress_bind(active, contracts, ledger, scope, profile, turn_profile).await);
        }
    };
    let continuation = observed
        .retired
        .and_then(|_| active.get(&scope.session_id))
        .filter(|turn| turn.owner.is_none() && turn.profile_id == turn_profile)
        .map(|turn| (turn.state.clone(), turn.interrupt_tx.clone()));
    drop(active);
    drop(grant_snapshot);
    if let Some((state, interrupt_tx)) = continuation {
        let _ = capture_turn_interrupt(state, interrupt_tx, InterruptOrigin::IngressRevoked).await;
    }
    Ok((observed.generation, observed.lifetime))
}
