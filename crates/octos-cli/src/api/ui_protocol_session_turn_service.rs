//! `SessionTurnService` (T-0636, `docs/IM_REMOTE_CONTROL.md` §14): the core
//! of `session/open`, `turn/start`, `turn/interrupt` and `approval/respond`,
//! taken out of the WebSocket handlers so a second ingress can drive the same
//! sessions without a socket. Each handler is now a thin adapter: it calls the
//! service with its connection as the [`TurnCaller`] and puts the outcome on
//! the wire.
//!
//! The service makes every decision the handlers made and changes the same
//! state in the same order: scope, ownership and host checks, turn admission
//! and the turn registries, the approval store, ledger appends. It returns
//! typed outcomes and the same `RpcError`s and never builds a JSON-RPC frame.
//! What only a transport does stays in the adapter: the `session/open` live
//! subscription, replay and forwarder, and answering a `turn/start` before
//! its turn may run (an accept the client never got aborts the turn).
//!
//! An admitted turn still runs in `run_standalone_turn`, which delivers its
//! events through a `WsConnection`; the caller supplies that runner
//! ([`TurnCaller::turn_runner`]). A runner that delivers elsewhere is later
//! work, not part of this extraction.

use super::*;

/// Whoever sent a request, as far as the service needs to know: who it is,
/// how a durable event reaches it, and what runs the turns it starts.
pub(super) trait TurnCaller {
    /// Runs this caller's admitted turns and delivers their events.
    type Runner: TurnRunner;

    /// Recorded as the owner of the turns and approvals this caller answers
    /// for, and stamped on the ledger rows it is sent directly.
    fn connection_id(&self) -> ConnectionId;

    /// An external client of `octos serve --host-managed` (UPCR-2026-036).
    fn is_external(&self) -> bool;

    /// The work-secret grant generation this caller was admitted under.
    fn ingress_generation(&self) -> Option<u64>;

    /// Append `notification` to the session ledger and deliver it to this
    /// caller, as `send_notification_durable` does for a socket.
    fn emit_durable(
        &self,
        ledger: &UiProtocolLedger,
        notification: UiNotification,
    ) -> Result<(), SendError>;

    /// Taken when a turn is admitted, run once its accept has been answered.
    fn turn_runner(&self) -> Self::Runner;
}

/// Runs one admitted turn.
pub(super) trait TurnRunner: Send + 'static {
    fn run(self, run: TurnRun) -> impl std::future::Future<Output = ()> + Send;
}

/// Everything an admitted turn runs with.
pub(super) struct TurnRun {
    pub(super) state: Arc<AppState>,
    pub(super) ledger: Arc<UiProtocolLedger>,
    pub(super) contracts: Arc<UiProtocolContractStores>,
    pub(super) background_work: Arc<crate::contracts::background_work::BackgroundWork>,
    pub(super) features: ConnectionUiFeatures,
    pub(super) params: TurnStartParams,
    pub(super) prompt: String,
    pub(super) pre_admitted_voice: Option<PreAdmittedVoice>,
    pub(super) resolved_profile_id: Option<String>,
    pub(super) turn_state: Arc<TokioMutex<TurnState>>,
    pub(super) interrupt_rx: mpsc::Receiver<()>,
    pub(super) steer_buffer: Option<octos_agent::SharedSteerBuffer>,
    pub(super) interactive_goal_binding: Option<(String, String)>,
    pub(super) turn_cancel: tokio_util::sync::CancellationToken,
    #[cfg(any(test, feature = "transport-test-fixtures"))]
    pub(super) fixture: Option<M9ProtocolFixture>,
}

impl TurnCaller for WsConnection {
    type Runner = WsConnection;

    fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }

    fn is_external(&self) -> bool {
        WsConnection::is_external(self)
    }

    fn ingress_generation(&self) -> Option<u64> {
        self.ingress_generation
    }

    fn emit_durable(
        &self,
        ledger: &UiProtocolLedger,
        notification: UiNotification,
    ) -> Result<(), SendError> {
        send_notification_durable(self, ledger, notification)
    }

    fn turn_runner(&self) -> WsConnection {
        self.clone()
    }
}

impl TurnRunner for WsConnection {
    async fn run(self, run: TurnRun) {
        #[cfg(any(test, feature = "transport-test-fixtures"))]
        if let Some(fixture) = run.fixture {
            run_m9_fixture_turn(
                self,
                run.state,
                run.ledger,
                run.contracts,
                run.params,
                fixture,
                run.resolved_profile_id,
                run.turn_state,
                run.interrupt_rx,
            )
            .await;
            return;
        }

        run_standalone_turn(
            self,
            run.state,
            run.ledger,
            run.contracts,
            run.background_work,
            run.features,
            run.params,
            run.prompt,
            run.pre_admitted_voice,
            run.resolved_profile_id,
            run.turn_state,
            run.interrupt_rx,
            run.steer_buffer,
            false,
            None,
            // #1133 — regular `turn/start` path is user-initiated;
            // it never runs as a goal continuation, so no goal
            // accountant wiring. Only the master continuation
            // runner sets this to `Some(...)` for `GoalContinue`.
            None,
            // #1650 — interactive goal binding snapshotted at admission.
            run.interactive_goal_binding,
            // #436 — regular user turns already persist their prompt
            // (they are not internal continuations); no override needed.
            false,
            // #436 P1 #2 — regular turns don't gate completion on dispatch.
            None,
            // OLP-CTRL 回合 4 — an interactive turn is never a steer
            // continuation turn; it must not consume reviewer-notes.
            false,
            None,
            run.turn_cancel,
        )
        .await;
    }
}

/// A refused request. A scope error goes out through `send_scope_error`,
/// which also closes an authenticated socket on an auth-scope violation;
/// any other is a plain error reply.
#[derive(Debug)]
pub(super) enum Refused {
    Scope(RpcError),
    Error(RpcError),
}

impl Refused {
    pub(super) fn send(self, ws: &WsConnection, id: String) {
        match self {
            Refused::Scope(error) => send_scope_error(ws, id, error),
            Refused::Error(error) => {
                let _ = send_rpc_error(ws, Some(id), error);
            }
        }
    }
}

/// A `turn/start` decided. The admission markers stay up until the caller has
/// answered it, so `turn/state/get` keeps calling the turn "being admitted"
/// until then (UPCR-2026-031).
pub(super) struct TurnStart {
    pub(super) outcome: Result<AdmittedTurn, Refused>,
    _admission: TurnAdmission,
    _folded_admission: Option<TurnAdmission>,
}

/// An admitted turn waiting for its accept to reach the caller.
pub(super) struct AdmittedTurn {
    session_id: SessionKey,
    turn_id: TurnId,
    turn_state: Arc<TokioMutex<TurnState>>,
    abort: AbortHandle,
    start: oneshot::Sender<Arc<crate::contracts::background_work::BackgroundWork>>,
    background_work: Arc<crate::contracts::background_work::BackgroundWork>,
}

impl AdmittedTurn {
    /// The accept reached the caller: let the turn run.
    pub(super) fn start(self) {
        let _ = self.start.send(self.background_work);
    }

    /// The accept could not be delivered: abort the freshly-inserted turn —
    /// running an unaccepted turn would be a leak.
    pub(super) async fn abandon(
        self,
        active_turns: &SharedActiveTurns,
        connection_turns: &SharedConnectionTurns,
    ) {
        let AdmittedTurn {
            session_id,
            turn_id,
            turn_state,
            abort,
            ..
        } = self;
        abort.abort();
        let mut active = active_turns.lock().await;
        if active
            .get(&session_id)
            .is_some_and(|entry| entry.turn_id == turn_id && Arc::ptr_eq(&entry.state, &turn_state))
        {
            active.remove(&session_id);
        }
        drop(active);
        let mut connection = connection_turns.lock().await;
        if connection.get(&session_id).is_some_and(|registered| {
            registered.turn_id == turn_id && Arc::ptr_eq(&registered.state, &turn_state)
        }) {
            connection.remove(&session_id);
        }
    }
}

/// An answered approval: the result to reply with, then the audit entry.
pub(super) struct ApprovalResponded {
    pub(super) result: Value,
    event: ApprovalDecidedEvent,
    tool_name: Option<String>,
}

impl ApprovalResponded {
    /// Append the decision to the durable audit log, after the reply is out.
    pub(super) async fn audit(&self, state: &AppState, contracts: &UiProtocolContractStores) {
        if let Some(sessions) = state.sessions.as_ref() {
            let data_dir = sessions.lock().await.data_dir();
            audit_approval_decided(contracts, &data_dir, &self.event, self.tool_name.as_deref());
        }
    }
}

pub(super) struct SessionTurnService<'a> {
    state: &'a Arc<AppState>,
    ledger: &'a Arc<UiProtocolLedger>,
}

impl<'a> SessionTurnService<'a> {
    pub(super) fn new(state: &'a Arc<AppState>, ledger: &'a Arc<UiProtocolLedger>) -> Self {
        Self { state, ledger }
    }

    /// The session a `session/open` targets: a `session_ref` resolved
    /// read-only, else the raw `session_id`.
    pub(super) fn open_target(
        &self,
        connection_profile_id: Option<&str>,
        params: &SessionOpenParams,
    ) -> SessionKey {
        let state = self.state;
        if let Some(reference) = params.session_ref.as_deref() {
            crate::api::workspace_registry::resolve_session_ref(
                state,
                connection_profile_id,
                reference,
            )
            .map(|record| SessionKey(record.session_id))
            .unwrap_or_else(|_| params.session_id.clone())
        } else {
            params.session_id.clone()
        }
    }

    /// Open `params` for `caller`: the session/open result, replay and
    /// pending prompts, and the workspace team join. `target` is
    /// [`Self::open_target`] of the same params.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn open(
        &self,
        caller: &impl TurnCaller,
        approvals: &PendingApprovalStore,
        questions: &PendingQuestionStore,
        connection_profile_id: Option<&str>,
        pinned_profile_id: Option<&str>,
        features: ConnectionUiFeatures,
        params: SessionOpenParams,
        session_ingress: bool,
        session_create: bool,
        target: &SessionKey,
    ) -> Result<SessionOpenOutcome, Refused> {
        let (state, ledger) = (self.state, self.ledger);
        let mut outcome = match open_session_result_with_mode(
            state,
            ledger,
            approvals,
            questions,
            caller.connection_id(),
            connection_profile_id,
            pinned_profile_id,
            features,
            params,
            session_create,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(error) => return Err(Refused::Scope(error)),
        };
        // A successful coding-session open joins its canonical workspace team.
        // App peers/context sessions keep their host-owned lifecycle and authority.
        if !session_ingress && peer_handoff_allowed_for_session(target) {
            let opened = &outcome.result.opened;
            if let (Some(runtime), Some(workspace)) = (
                resolve_session_profile_runtime(state, opened.active_profile_id.as_deref()),
                opened.workspace_root.as_deref(),
            ) {
                // Eviction must not take the seat of a session whose turn is
                // still able to record a result into the team (#2741).
                let turn_active = active_turn_sessions(&active_turns_registry()).await;
                if let Err(error) = state.ui_protocol.workspace_teams.join(
                    &runtime.data_dir,
                    Path::new(workspace),
                    &opened.session_id,
                    caller.connection_id().0,
                    &turn_active,
                ) {
                    // The join failure degrades to "opened, outside the team"
                    // (#2741): a fail-closed refusal here bricks long-lived
                    // workspaces — the lifetime member accumulation + the 1024
                    // hard cap mean every open past the cap permanently fails.
                    // The session stays fully usable (the memory/skills/history
                    // bindings are durable, not team-gated); the team features
                    // (shared history, team messaging) are absent, and the
                    // operator sees the reason in the logs.
                    tracing::error!(
                        session = %opened.session_id,
                        workspace = %workspace,
                        %error,
                        "workspace team join failed; the session opened outside the team"
                    );
                }
            }
        }

        // #1594 follow-up: a session-ingress connection may only call the
        // session-scoped surface, so the SessionOpened reply it receives must not
        // advertise the raw/global methods it would be denied. Filter only this
        // direct reply; the ledger-broadcast copy other connections observe is
        // untouched (it reflects the session's negotiated features, not this
        // credential's scope).
        if session_ingress {
            filter_capabilities_for_session_ingress(&mut outcome.result.opened.capabilities);
        }
        // #1594 follow-up: the SessionOpened NOTIFICATION is a documented
        // capability-discovery path (UPCR-2026-007), so an ingress connection's
        // direct-sent copy must be filtered just like the RPC result was. This
        // mutates only the per-connection clone in `outcome`; the shared ledger
        // entry (already appended, broadcast to other connections) is untouched, and
        // `send_ledger_event_durable` sends without re-appending.
        if session_ingress {
            if let UiProtocolLedgerEvent::Notification(UiNotification::SessionOpened(opened)) =
                &mut outcome.opened_event.event
            {
                filter_capabilities_for_session_ingress(&mut opened.capabilities);
            }
        }
        Ok(outcome)
    }

    /// Decide a `turn/start` for `caller` and, when admitted, register the
    /// turn. It runs only once [`AdmittedTurn::start`] is called.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn start_turn<C: TurnCaller>(
        &self,
        caller: &C,
        contracts: &Arc<UiProtocolContractStores>,
        active_turns: &SharedActiveTurns,
        connection_turns: &SharedConnectionTurns,
        connection_profile_id: Option<&str>,
        routed_profile_id: Option<&str>,
        features: ConnectionUiFeatures,
        mut params: TurnStartParams,
        pre_admitted_voice: Option<PreAdmittedVoice>,
    ) -> TurnStart {
        // UPCR-2026-031: while this start is being admitted (it is not in the
        // registry yet), `turn/state/get` must not report the turn as certainly
        // not running. Keyed by the ids exactly as the client sent them, which is
        // how it later asks about the turn.
        let _admission = TurnAdmission::enter(&params.session_id, &params.turn_id);
        let raw_session_id = params.session_id.clone();
        // UPCR-2026-015 (M9-β-1): if the client carried a `topic` field
        // alongside the session_id, fold it into the resolved SessionKey
        // BEFORE scope validation. The rest of the turn pipeline keys
        // exclusively off `params.session_id`, so adopting the topic-
        // suffixed form here means history lookup, ledger appends, and
        // `task/list` filtering all see the per-topic bucket
        // automatically. Empty / whitespace-only topics fall through to
        // the bare session shape (matching `SessionKey::with_topic`'s
        // own empty-string short-circuit).
        if let Some(topic) = params
            .topic
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            // Replace the SessionKey with the topic-suffixed form. Splice
            // any existing topic suffix away first so a client that sends
            // both `session_id: "x:y#old"` and `topic: "new"` lands in a
            // single, unambiguous bucket (`x:y#new`) rather than the
            // double-suffixed garbage `x:y#old#new`. The base parser
            // already handles `#`-stripped lookups, but we want the
            // canonical form on the wire-trip back to clients.
            let base = params.session_id.base_key().to_owned();
            params.session_id = SessionKey(format!("{base}#{topic}"));
        }
        // UPCR-2026-031: `turn/state/get` takes no topic, so a topic client asks
        // by the FOLDED id the registry uses — mark that key as admitting too.
        let _folded_admission = (params.session_id.0 != raw_session_id.0)
            .then(|| TurnAdmission::enter(&params.session_id, &params.turn_id));
        #[cfg(test)]
        turn_admission_test_pause(&params.turn_id).await;

        let outcome = self
            .admit_turn(
                caller,
                contracts,
                active_turns,
                connection_turns,
                connection_profile_id,
                routed_profile_id,
                features,
                params,
                pre_admitted_voice,
            )
            .await;
        TurnStart {
            outcome,
            _admission,
            _folded_admission,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn admit_turn<C: TurnCaller>(
        &self,
        caller: &C,
        contracts: &Arc<UiProtocolContractStores>,
        active_turns: &SharedActiveTurns,
        connection_turns: &SharedConnectionTurns,
        connection_profile_id: Option<&str>,
        routed_profile_id: Option<&str>,
        features: ConnectionUiFeatures,
        params: TurnStartParams,
        pre_admitted_voice: Option<PreAdmittedVoice>,
    ) -> Result<AdmittedTurn, Refused> {
        let (state, ledger) = (self.state, self.ledger);
        if let Err(error) = validate_session_scope(&params.session_id, None, connection_profile_id)
        {
            return Err(Refused::Scope(error));
        }
        // The shared peer conversation: who speaks in this turn of a host-owned
        // app peer's own session (decided before the `peer/input` claim below,
        // so a refused relabel does not answer the input). The kernel's marker
        // leads the prompt, so the model, the transcript and history/replay all
        // carry it.
        let peer_turn_origin = match resolve_peer_turn_origin(
            state,
            caller,
            &params,
            connection_profile_id.or(routed_profile_id),
        ) {
            Ok(origin) => origin,
            Err(error) => return Err(Refused::Error(error)),
        };
        // Refuse early a turn id whose input the host already refused; the input
        // itself is answered only once the turn is admitted (below), so a start
        // refused for any other reason leaves it open for `peer/input/reject`.
        if let Err(error) = check_peer_input_turn(state, &params.session_id, &params.turn_id) {
            return Err(Refused::Error(error));
        }

        let prompt = match prompt_text(&params.input) {
            Some(p) => p,
            None => {
                // Voice turns carry audio media and NO text item — the
                // serve-path STT inside `run_standalone_turn` transcribes the
                // audio into the prompt. Accept such a turn with an empty
                // prompt; reject only when there is neither text nor audio to
                // act on (the original "requires a text input item" contract
                // for text-only clients).
                if params
                    .media
                    .iter()
                    .any(|m| octos_bus::media::is_audio(&m.path))
                {
                    String::new()
                } else {
                    return Err(Refused::Error(RpcError::invalid_params(
                        "turn/start requires at least one text input item",
                    )));
                }
            }
        };
        let prompt = match peer_turn_origin
            .as_ref()
            .and_then(|decided| decided.origin.as_ref())
        {
            Some(origin) => crate::peers::turn_origin::label_prompt(origin, &prompt),
            None => prompt,
        };

        #[cfg(any(test, feature = "transport-test-fixtures"))]
        let fixture = m9_protocol_fixture_for_prompt(&prompt);
        let mut background_profile = None;
        #[cfg(any(test, feature = "transport-test-fixtures"))]
        let is_fixture = fixture.is_some();
        #[cfg(not(any(test, feature = "transport-test-fixtures")))]
        let is_fixture = false;
        if !is_fixture {
            // M11-F: validate that a `ProfileRuntime` is registered for the
            // routed profile BEFORE spawning the turn task. The legacy
            // `validate_runtime` (which checked `state.agent` /
            // `state.sessions`) was deleted; the equivalent gate now is
            // "the SessionRuntimeCache can resolve a ProfileRuntime for
            // this session's profile id". Fail fast with the same
            // `runtime_unavailable` shape so existing clients see no wire
            // change.
            let active_profile_id = params
                .session_id
                .profile_id()
                .map(ToOwned::to_owned)
                .or_else(|| {
                    connection_profile_id
                        .or(routed_profile_id)
                        .map(ToOwned::to_owned)
                })
                // #2162: a raw id on an unscoped, un-routed connection — the
                // profile the session's `session/open` resolved under.
                .or_else(|| session_open_profiles().lookup(&params.session_id));
            if let Some(profile_id) = active_profile_id.as_deref() {
                if let Err(error) = ensure_known_profile(state, profile_id) {
                    return Err(Refused::Error(error));
                }
            }
            let Some(profile_runtime) =
                resolve_session_profile_runtime(state, active_profile_id.as_deref())
            else {
                return Err(Refused::Error(runtime_unavailable_error(
                    profile_runtime_unavailable_message(
                        state,
                        active_profile_id.as_deref().unwrap_or("<unset>"),
                    ),
                )));
            };
            background_profile = Some(profile_runtime.profile_id.clone());
            if let Err(error) = require_recovered_scoped_session_open(
                state,
                ledger,
                &params.session_id,
                &profile_runtime.profile_id,
            ) {
                return Err(Refused::Error(error));
            }
        }

        let state_for_turn = state.clone();
        let ledger_for_turn = ledger.clone();
        let contracts_for_turn = contracts.clone();
        let session_id = params.session_id.clone();
        let turn_id = params.turn_id.clone();
        let turn_state = Arc::new(TokioMutex::new(TurnState::Active));
        let (interrupt_tx, interrupt_rx) = mpsc::channel::<()>(1);
        let interrupt_tx = Arc::new(TokioMutex::new(Some(interrupt_tx)));
        let turn_state_for_task = turn_state.clone();
        let (start_tx, start_rx) = tokio::sync::oneshot::channel();
        // `turn/steer` pending-input buffer (codex `TurnState.pending_input`).
        // Regular standalone turns are steerable; M9 protocol fixture turns are
        // not (they never run the agent loop, so a pushed input would silently
        // vanish — advertise that by registering no buffer).
        let steer_buffer: Option<octos_agent::SharedSteerBuffer> =
            (!is_fixture).then(|| Arc::new(octos_agent::SteerBuffer::default()));
        let steer_buffer_for_turn = steer_buffer.clone();
        let resolved_profile_id = connection_profile_id
            .or(routed_profile_id)
            .map(ToOwned::to_owned);
        // Stamp for the ActiveTurn registry: mirror `session/btw`'s resolution
        // (session key first) so the aside's profile check compares like with
        // like; canonical `_main` when nothing resolves.
        let profile_for_stamp = session_id
            .profile_id()
            .map(ToOwned::to_owned)
            .or_else(|| resolved_profile_id.clone())
            .unwrap_or_else(|| MAIN_PROFILE_ID.to_owned());
        // #1650 — snapshot the interactive goal binding NOW, synchronously,
        // at dispatch time — before the turn task is spawned and woken via
        // `start_rx`. A `session/goal/set` that arrives after this `turn/start`
        // (same or another connection) therefore can't bind this already-
        // submitted turn to a goal created after the fact (codex P2).
        //
        // Resolve the profile through the SAME `resolve_autonomy_profile_id`
        // the raw `goal/set` handler uses (codex P2), so the binding matches
        // exactly where the goal was stored. `profile_for_stamp` was subtly
        // different — it falls back to the routed profile, whereas `goal/set`
        // for an unscoped connection on an unprofiled session resolves to
        // `_main` — which left accounting silently disabled for that flow. A
        // scope error (unreachable here — `handle_turn_start` already ran
        // `validate_session_scope` with the same inputs) or a session with no
        // active goal both bind to `None` and never charge.
        //
        // KNOWN LIMITATION (codex P2): a `turn/start` carries no goal
        // profile, so when an unscoped/admin connection sets a goal with an
        // EXPLICIT `profile_id` on a bare session key, this resolves to
        // `_main` and misses the tenant-scoped goal — that turn goes
        // uncharged. Binding by session-key alone (ignoring profile) would
        // fix it but risks the cross-tenant leak the profile match exists to
        // prevent, since "unscoped ⇒ authorized for any profile" is
        // deployment-dependent. A safe fix threads the goal's stored profile
        // (or an authorized bind-by-session for admin connections) and is
        // tracked as a follow-up.
        let interactive_goal_binding =
            resolve_autonomy_profile_id(Some(&session_id), None, connection_profile_id)
                .ok()
                .and_then(|goal_profile| {
                    default_agent_orchestrator()
                        .active_goal_id(&session_id, &goal_profile)
                        .map(|goal_id| (goal_profile, goal_id))
                });
        let cache_reservation = match reserve_peer_build_cache_turn(
            state,
            &session_id,
            &turn_id,
            &turn_state,
            resolved_profile_id.as_deref(),
        ) {
            Ok(reservation) => reservation,
            Err(error) => return Err(Refused::Error(error)),
        };
        let prompt_for_turn = prompt.clone();
        // UPCR-2026-R4946304A (T-0241): one token per admitted turn — cloned
        // into the agent (tool workers watch it) and into the ActiveTurn entry
        // (abort paths fire it).
        let turn_cancel = tokio_util::sync::CancellationToken::new();
        let turn_cancel_for_agent = turn_cancel.clone();
        let runner = caller.turn_runner();
        let handle = tokio::spawn(async move {
            let _cache_reservation = cache_reservation;
            let Ok(background_work) = start_rx.await else {
                return;
            };
            runner
                .run(TurnRun {
                    state: state_for_turn,
                    ledger: ledger_for_turn,
                    contracts: contracts_for_turn,
                    background_work,
                    features,
                    params,
                    prompt: prompt_for_turn,
                    pre_admitted_voice,
                    resolved_profile_id,
                    turn_state: turn_state_for_task,
                    interrupt_rx,
                    steer_buffer: steer_buffer_for_turn,
                    interactive_goal_binding,
                    turn_cancel: turn_cancel_for_agent,
                    #[cfg(any(test, feature = "transport-test-fixtures"))]
                    fixture,
                })
                .await;
        });

        // Serialize observation only within this session/profile. Different
        // sessions retain independent storage and admission progress. Read the
        // fenced snapshot before the global admission lock, then keep its guard
        // through observation, background-work admission, and ActiveTurn insert.
        let grant_profile = background_profile
            .as_deref()
            .unwrap_or(&profile_for_stamp)
            .to_owned();
        let grant_guard = lock_background_grant_observation(&session_id, &grant_profile).await;
        let background_grants = load_background_grants(state).await;
        await_ingress_bind_admission_gate(&session_id).await;
        // `None` => admitted. `Some(refusal)` => refused; an occupied session
        // carries the id of the turn that actually holds it. The id is captured in the SAME
        // lock scope that makes the decision (never by re-acquiring the registry
        // afterwards, which could name a different turn) and it costs no new
        // await under the lock — it is a clone of a field already in hand.
        let admission = {
            let mut active = active_turns.lock().await;
            // Allow replacing a `Terminal(_)` entry — the prior turn is finished;
            // we keep the entry only so a follow-up `turn/interrupt` can return
            // `terminal_state` instead of `unknown_turn`. Any non-terminal entry
            // means there is still a turn running for this session.
            let occupied = turn_admission_refusal(
                &active,
                &session_id,
                &turn_id,
                state.host_managed.is_some(),
            )
            .await;
            // UPCR-2026-035: the turn is admitted, so it now answers the
            // `peer/input` that handed its id out — decided under the same lock,
            // so a concurrent `peer/input/reject` either lands first (and this
            // start is refused) or finds the input started.
            let admission = match occupied {
                Some(refusal) => Err(refusal.into_error(caller.is_external())),
                None => Ok(()),
            };
            let admission = admission.and_then(|()| {
                let profile = background_profile.as_deref().unwrap_or(&profile_for_stamp);
                let fail_closed_on_error = caller.ingress_generation().is_some();
                let _observed = observe_background_grant_read(
                    &background_grants,
                    contracts,
                    ledger,
                    &session_id,
                    profile,
                    None,
                    fail_closed_on_error,
                )?;
                let work = contracts
                    .background_work
                    .admit(&session_id, profile, &turn_id, caller.ingress_generation())
                    .ok_or_else(|| {
                        RpcError::invalid_request(
                            "Session ingress generation was revoked; reconnect with a new grant",
                        )
                    })?;
                claim_peer_input_turn(state, &session_id, &turn_id)?;
                Ok(work)
            });
            if admission.is_ok() {
                // Client-supplied turn ids carry no uniqueness guarantee — a
                // reused id must not inherit a prior turn's `session/btw` draft.
                btw_live_draft_clear(&session_id, &turn_id);
                active.insert(
                    session_id.clone(),
                    ActiveTurn {
                        dispatched_prompt: None,
                        turn_id: turn_id.clone(),
                        profile_id: profile_for_stamp.clone(),
                        state: turn_state.clone(),
                        interrupt_tx,
                        steer: steer_buffer,
                        abort: handle.abort_handle(),
                        turn_cancel: turn_cancel.clone(),
                        owner: Some(caller.connection_id()),
                    },
                );
            }
            admission
        };
        // The block above has released active_turns; only now release the
        // cross-process snapshot fence and the per-session observation gate.
        drop(background_grants);
        drop(grant_guard);
        let background_work = match admission {
            Ok(work) => work,
            Err(error) => {
                handle.abort();
                return Err(Refused::Error(error));
            }
        };
        // The first transcript row is still owned by the ordinary commit path,
        // but session/list must already be able to title and sort this admitted
        // turn while it is live (including the approval wait before that commit).
        if let Some(error) = note_first_turn_discovery(
            state,
            connection_profile_id,
            routed_profile_id,
            &session_id,
            &prompt,
        )
        .await
        {
            handle.abort();
            active_turns.lock().await.remove(&session_id);
            return Err(Refused::Error(error));
        }
        // Admitted: remember who speaks (the turn's terminal labels its
        // blackboard result with it, and a person's question does not wake the
        // system agent).
        if let Some(decided) = &peer_turn_origin {
            match &decided.origin {
                Some(origin) => crate::peers::turn_origin::record_turn_origin(
                    &session_id,
                    &turn_id,
                    origin.clone(),
                ),
                None => crate::peers::turn_origin::clear_turn_origin(&session_id),
            }
        }

        let abort = handle.abort_handle();
        connection_turns.lock().await.insert(
            session_id.clone(),
            ConnectionTurn {
                turn_id: turn_id.clone(),
                state: turn_state.clone(),
                finished: async move {
                    let _ = handle.await;
                }
                .boxed()
                .shared(),
            },
        );
        Ok(AdmittedTurn {
            session_id,
            turn_id,
            turn_state,
            abort,
            start: start_tx,
            background_work,
        })
    }

    /// Decide a `turn/interrupt` for `caller`, waiting for a captured turn's
    /// acknowledgement (bounded by `INTERRUPT_ACK_TIMEOUT`).
    pub(super) async fn interrupt(
        &self,
        caller: &impl TurnCaller,
        active_turns: &SharedActiveTurns,
        connection_profile_id: Option<&str>,
        params: TurnInterruptParams,
    ) -> Result<TurnInterruptResult, RpcError> {
        let state = self.state;
        // task-turn-interrupt-steer-correlation-logs: make the interrupt's
        // receipt, decision and ack reconstructible from the log alone.
        crate::turn_trace::log_interrupt_received(&params.session_id, &params.turn_id);
        if let Some(error) =
            refuse_foreign_host_turn_control(state, &params.session_id, caller, "turn/interrupt")
        {
            return Err(error);
        }
        let required_owner = caller.is_external().then(|| caller.connection_id());
        let outcome = decide_interrupt_as(active_turns, &params, required_owner).await;
        let outcome_label: String = match &outcome {
            InterruptOutcome::Unknown => "unknown".into(),
            InterruptOutcome::Mismatch => "mismatch".into(),
            InterruptOutcome::NotOwner => "not_owner".into(),
            InterruptOutcome::AlreadyTerminal(reason) => {
                format!("already_terminal:{}", reason.as_str())
            }
            InterruptOutcome::AlreadyInterrupting => "already_interrupting".into(),
            InterruptOutcome::Captured { .. } => "captured".into(),
        };
        crate::turn_trace::log_interrupt_outcome(
            &params.session_id,
            &params.turn_id,
            &outcome_label,
        );
        match outcome {
            InterruptOutcome::Unknown => {
                let error = match deleted_session_state(
                    state,
                    connection_profile_id,
                    None,
                    &params.session_id,
                )
                .await
                {
                    Ok(true) => RpcError::unknown_session(params.session_id.0.clone()),
                    Ok(false) => unknown_turn_error(&params.turn_id),
                    Err(error) => error,
                };
                Err(error)
            }
            InterruptOutcome::NotOwner => Err(crate::api::host_managed::external_turn_denied(
                "turn/interrupt",
            )),
            InterruptOutcome::Mismatch => {
                // Codified by accepted UPCR-2026-008: typed `reason` field on
                // `TurnInterruptResult`. String registry value `turn_id_mismatch`.
                Ok(TurnInterruptResult::declined("turn_id_mismatch"))
            }
            InterruptOutcome::AlreadyTerminal(reason) => {
                let interrupted = matches!(reason, TerminalReason::Interrupted);
                // Codified by accepted UPCR-2026-008: typed `terminal_state` field
                // on `TurnInterruptResult`. Values come from `TerminalReason`.
                Ok(TurnInterruptResult::already_terminal(
                    reason.as_str(),
                    interrupted,
                ))
            }
            InterruptOutcome::AlreadyInterrupting => {
                // A prior caller transitioned the turn to `Interrupting` and is
                // awaiting ack. The terminal event is already guaranteed to be
                // emitted exactly once. Idempotent: report the same response shape
                // as the original caller will.
                Ok(TurnInterruptResult::interrupted_ok())
            }
            InterruptOutcome::Captured { ack_rx } => {
                // State is now `Interrupting { ack }`; the turn task is wired to
                // observe `interrupt_rx`, abort its agent, emit exactly one
                // `TurnError(interrupted)`, and signal `ack`. We do NOT abort the
                // outer turn future here — that would race with the terminal
                // emission and could lose the wire-side event.
                let result = tokio::time::timeout(INTERRUPT_ACK_TIMEOUT, ack_rx).await;
                crate::turn_trace::log_interrupt_ack(
                    &params.session_id,
                    &params.turn_id,
                    if matches!(result, Ok(Ok(()))) {
                        "interrupted"
                    } else {
                        "ack_timed_out"
                    },
                );
                let payload = match result {
                    Ok(Ok(())) => TurnInterruptResult::interrupted_ok(),
                    Ok(Err(_)) => {
                        // Sender dropped without ack — the task panicked or was
                        // cancelled before reaching the terminal arm. The state
                        // remains `Interrupting`; report timeout-style result so
                        // the caller knows the wire-side terminal is uncertain.
                        // Codified by accepted UPCR-2026-008.
                        TurnInterruptResult::ack_timed_out()
                    }
                    Err(_) => TurnInterruptResult::ack_timed_out(),
                };
                Ok(payload)
            }
        }
    }

    /// Answer an `approval/respond` for `caller`: the decided event is
    /// appended and delivered before the result is returned.
    /// `external_owner` is the caller's connection when it is an external
    /// client allowed to answer only its own turns' approvals.
    pub(super) async fn respond_approval(
        &self,
        caller: &impl TurnCaller,
        contracts: &Arc<UiProtocolContractStores>,
        connection_profile_id: Option<&str>,
        external_owner: Option<ConnectionId>,
        mut params: octos_core::ui_protocol::ApprovalRespondParams,
    ) -> Result<ApprovalResponded, Refused> {
        let (state, ledger) = (self.state, self.ledger);
        if let Err(error) = validate_session_scope(&params.session_id, None, connection_profile_id)
        {
            return Err(Refused::Scope(error));
        }
        // `octos serve --host-managed`: a host-owned app peer's approvals belong
        // to the person, in the app (UPCR-2026-034). An external client (web or
        // terminal UI on the external token) never answers them; the approval
        // stays parked. UPCR-2026-036.
        if let Some(owner) = external_owner {
            if crate::api::host_managed::is_peer_session(&params.session_id) {
                return Err(Refused::Error(
                    crate::api::host_managed::peer_answer_denied("approval"),
                ));
            }
            // Only an approval raised by a turn this external connection owns
            // (recorded on the approval, never inferred from a client-chosen turn
            // id), and once: an external answer never records a session-wide
            // scope.
            let own = contracts
                .approvals
                .pending_owner(&params.session_id, &params.approval_id)
                == Some(Some(owner.0));
            if !own {
                return Err(Refused::Error(
                    crate::api::host_managed::external_turn_denied("approval"),
                ));
            }
            params.approval_scope = None;
        }
        // UPCR-2026-036 (OctoSense ADR 0004 G1): an external client's approval
        // is answered by that client only, never by the host (whose automation
        // must not decide for an external client) or any other connection.
        if !crate::api::host_managed::external_prompt_visible(
            &params.approval_id.0.to_string(),
            caller.connection_id().0,
        ) || contracts
            .approvals
            .external_owner(&params.approval_id)
            .is_some_and(|owner| owner != Some(caller.connection_id().0))
        {
            return Err(Refused::Error(
                crate::api::host_managed::external_approval_owner_only(),
            ));
        }
        // UPCR-2026-035: a host-routed call's approval is answered only on the
        // connection that raised it or the peer's current host connection.
        // Every other approval on a host peer's session (a kernel tool's, in a
        // host-driven turn) likewise: its owning connection or the host's.
        let host_owner_ok = match contracts.approvals.host_route_owner(&params.approval_id) {
            Some((route, raised_on)) => crate::peers::host_tools::host_approval_answerable(
                &route,
                raised_on,
                caller.connection_id().0,
            ),
            None => host_session_answer_allowed(
                state,
                &params.session_id,
                contracts
                    .approvals
                    .pending_owner(&params.session_id, &params.approval_id)
                    .flatten(),
                caller,
            ),
        };
        if !host_owner_ok
            || !crate::peers::host_tools::host_approval_visible(
                &params.approval_id.0.to_string(),
                caller.connection_id().0,
            )
        {
            return Err(Refused::Error(host_connection_only_error(
                "approval/respond",
            )));
        }

        let session_id = params.session_id.clone();
        let scope_string = params.approval_scope.clone();
        // FIX-01: `ApprovalDecision` is non-Copy because of the `Unknown(String)`
        // variant; clone to keep the value alive across `respond_with_context`
        // (consumes `params` via clone), the scope-recording call below, and the
        // FIX-07 audit/notification emission.
        let decision = params.decision.clone();

        let mut decided_event = None;
        let outcome =
            match contracts
                .approvals
                .respond_with_context_and_persist(params.clone(), |outcome| {
                    let tool_name = outcome.context.as_ref().map(|ctx| ctx.tool_name.clone());
                    let event = crate::api::ui_protocol_approvals::build_decided_event(
                        &params,
                        outcome,
                        connection_profile_id.unwrap_or(""),
                        Utc::now(),
                    );
                    emit_approval_decided(caller, ledger, &event, tool_name.as_deref())?;
                    decided_event = Some(event);
                    Ok::<(), SendError>(())
                }) {
                Ok(outcome) => outcome,
                Err(crate::contracts::approvals::ApprovalRespondPersistError::Rpc(error)) => {
                    return Err(Refused::Error(error));
                }
                Err(crate::contracts::approvals::ApprovalRespondPersistError::Persist(error)) => {
                    return Err(Refused::Error(RpcError::internal_error(format!(
                        "approval/decided durable append failed: {error:?}"
                    ))));
                }
            };
        let tool_name = outcome.context.as_ref().map(|ctx| ctx.tool_name.clone());
        let event = decided_event.expect("successful guarded respond records its decided event");

        // FIX-06: if the user picked a recordable scope and we have the original
        // request context, register the policy entry. Open-registry rule:
        // unknown scope strings collapse to `approve_once` and are not recorded
        // — preserving backward compat with clients that send future scope
        // tokens we don't yet recognise.
        record_approval_scope(
            contracts,
            &session_id,
            scope_string.as_deref(),
            outcome.context.as_ref(),
            decision,
        );

        let result = match serde_json::to_value(&outcome.result) {
            Ok(value) => value,
            Err(error) => {
                return Err(Refused::Error(RpcError::internal_error(format!(
                    "failed to serialize approval/respond result: {error}"
                ))));
            }
        };
        Ok(ApprovalResponded {
            result,
            event,
            tool_name,
        })
    }
}

#[cfg(test)]
#[path = "ui_protocol_session_turn_service_tests.rs"]
mod tests;
