//! ACP v1 presentation and request mapping over the shared OUP runtime.

use super::*;
#[cfg(test)]
#[path = "oup_tests.rs"]
mod tests;
use crate::commands::oup_session::{OupFrontend, OupSession};
use crate::commands::oup_text::AssistantTextProjection;
use octos_core::ui_protocol::{
    ApprovalDecision, ApprovalRespondParams, EnvelopeToolEndStatus, PayloadV2, UiCommand,
    UiNotification,
};

struct ProtocolSession {
    oup: OupSession,
    cancelled: PromptCancellation,
    busy: AtomicBool,
    busy_watch: tokio::sync::watch::Sender<bool>,
    idle_started: AtomicBool,
    segments: std::sync::Mutex<AssistantTextProjection>,
    peers: crate::commands::oup_peers::OupPeerHost,
}

/// Each prompt owns its cancellation flag. An interrupt RPC acknowledged
/// after the next prompt starts must not cancel that newer prompt.
#[derive(Default)]
struct PromptCancellation(std::sync::Mutex<Arc<AtomicBool>>);

impl PromptCancellation {
    fn begin(&self) -> Arc<AtomicBool> {
        let flag = Arc::new(AtomicBool::new(false));
        *self.0.lock().unwrap() = flag.clone();
        flag
    }

    fn current(&self) -> Arc<AtomicBool> {
        self.0.lock().unwrap().clone()
    }
}

type SessionSlot = Arc<tokio::sync::OnceCell<Arc<ProtocolSession>>>;
type Sessions = Arc<Mutex<HashMap<SessionId, SessionSlot>>>;

async fn open(
    factory: &dyn SessionAgentFactory,
    sessions: &Sessions,
    id: SessionId,
    cwd: PathBuf,
) -> std::result::Result<Arc<ProtocolSession>, AcpError> {
    let slot = sessions.lock().await.entry(id.clone()).or_default().clone();
    slot.get_or_try_init(|| async {
        let cwd = if cwd.as_os_str().is_empty() {
            factory.default_cwd().to_owned()
        } else {
            cwd
        };
        let state = factory.oup_state().await.map_err(internal)?;
        let key = SessionKey::with_profile(&factory.session_profile_id(), "acp", id.0.as_ref());
        // ACP v1 has request_permission, but no standard structured question
        // response. Do not negotiate OUP's blocking question extension; the
        // common backend returns the normal unsupported-client tool result.
        let oup = OupSession::open_with_questions(
            state.clone(),
            key,
            &cwd,
            octos_agent::EffectivePermissions::workspace_write(),
            false,
        )
        .await
        .map_err(internal)?;
        Ok(Arc::new(ProtocolSession {
            oup,
            cancelled: PromptCancellation::default(),
            busy: AtomicBool::new(false),
            busy_watch: tokio::sync::watch::channel(false).0,
            idle_started: AtomicBool::new(false),
            segments: std::sync::Mutex::new(AssistantTextProjection::default()),
            peers: crate::commands::oup_peers::OupPeerHost::new(
                state,
                octos_agent::EffectivePermissions::workspace_write(),
            ),
        }))
    })
    .await
    .cloned()
}

fn internal(error: impl std::fmt::Display) -> AcpError {
    agent_client_protocol::util::internal_error(error.to_string())
}

struct BusyTurn(Arc<ProtocolSession>);
impl Drop for BusyTurn {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::Release);
        self.0.busy_watch.send_replace(false);
    }
}

fn start_idle_updates(
    session: Arc<ProtocolSession>,
    id: SessionId,
    cx: ConnectionTo<Client>,
    stop: tokio_util::sync::CancellationToken,
) {
    if session.idle_started.swap(true, Ordering::AcqRel) {
        return;
    }
    tokio::spawn(async move {
        let mut busy = session.busy_watch.subscribe();
        let frontend = Frontend {
            id,
            cx,
            session: session.clone(),
        };
        loop {
            if *busy.borrow_and_update() {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    result = busy.changed() => { if result.is_err() { break; } }
                }
                continue;
            }
            tokio::select! {
                _ = stop.cancelled() => break,
                result = busy.changed() => { if result.is_err() { break; } }
                result = session.oup.listen(&frontend) => {
                    if let Err(error) = result { tracing::warn!(%error, "ACP idle OUP stream closed"); }
                    break;
                }
            }
        }
    });
}

struct Frontend {
    id: SessionId,
    cx: ConnectionTo<Client>,
    session: Arc<ProtocolSession>,
}

impl Frontend {
    fn send(&self, update: SessionUpdate) -> Result<()> {
        self.cx
            .send_notification(SessionNotification::new(self.id.clone(), update))
            .map_err(|error| eyre::eyre!("ACP notification failed: {error}"))
    }

    fn payload(&self, payload: PayloadV2) -> Result<()> {
        let update = match payload {
            PayloadV2::AssistantDelta {
                text,
                assistant_segment_id,
            } => {
                let text = self
                    .session
                    .segments
                    .lock()
                    .unwrap()
                    .delta(&assistant_segment_id, &text);
                (!text.is_empty()).then(|| {
                    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(text)))
                })
            }
            PayloadV2::AssistantPersisted {
                text,
                assistant_segment_id,
                ..
            } => {
                let tail = self
                    .session
                    .segments
                    .lock()
                    .unwrap()
                    .persisted(&assistant_segment_id, &text);
                (!tail.is_empty()).then(|| {
                    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(
                        tail.to_owned(),
                    )))
                })
            }
            PayloadV2::ReasoningDelta { text } => Some(SessionUpdate::AgentThoughtChunk(
                ContentChunk::new(ContentBlock::from(text)),
            )),
            PayloadV2::ToolStart {
                tool_call_id, name, ..
            } => Some(SessionUpdate::ToolCall(
                ToolCall::new(tool_call_id, name.clone())
                    .kind(tool_kind_for(&name))
                    .status(ToolCallStatus::InProgress),
            )),
            PayloadV2::ToolEnd {
                tool_call_id,
                status,
                output_preview,
                error,
                ..
            } => {
                let status = if status == EnvelopeToolEndStatus::Complete {
                    ToolCallStatus::Completed
                } else {
                    ToolCallStatus::Failed
                };
                let mut fields = ToolCallUpdateFields::new().status(status);
                if let Some(text) = output_preview.or(error) {
                    fields = fields.content(vec![ToolCallContent::from(ContentBlock::from(text))]);
                }
                Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    tool_call_id,
                    fields,
                )))
            }
            _ => None,
        };
        if let Some(update) = update {
            self.send(update)?;
        }
        Ok(())
    }

    fn replay(&self, history: octos_core::ui_protocol::SessionHydrateResult) -> Result<()> {
        let messages = history.messages.unwrap_or_default();
        let last_answers: HashMap<_, _> = messages
            .iter()
            .filter(|message| message.role == "assistant")
            .filter_map(|message| {
                message
                    .thread_id
                    .as_ref()
                    .map(|thread| (thread.clone(), message.seq))
            })
            .collect();
        let mut tools = history.replayed_tool_envelopes.unwrap_or_default();
        for message in messages {
            // Hydrate supplies canonical tool envelopes separately from rows.
            // Keep each thread's tools before its final answer, not below the
            // entire conversation. Do not reconstruct tool calls from prose.
            if message
                .thread_id
                .as_ref()
                .is_some_and(|thread| last_answers.get(thread) == Some(&message.seq))
            {
                let (matching, remaining): (Vec<_>, Vec<_>) = tools
                    .into_iter()
                    .partition(|envelope| Some(&envelope.thread_id) == message.thread_id.as_ref());
                tools = remaining;
                for envelope in matching {
                    self.payload(envelope.payload)?;
                }
            }
            for update in replay_message(message) {
                self.send(update)?;
            }
        }
        for envelope in tools {
            self.payload(envelope.payload)?;
        }
        Ok(())
    }
}

fn replay_message(message: octos_core::ui_protocol::HydratedMessage) -> Vec<SessionUpdate> {
    let mut updates = Vec::new();
    if message.role == "assistant"
        && let Some(reasoning) = message
            .reasoning_content
            .filter(|text| !text.trim().is_empty())
    {
        updates.push(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            ContentBlock::from(reasoning),
        )));
    }
    if !message.content.trim().is_empty() {
        let chunk = ContentChunk::new(ContentBlock::from(message.content));
        match message.role.as_str() {
            "user" => updates.push(SessionUpdate::UserMessageChunk(chunk)),
            "assistant" => updates.push(SessionUpdate::AgentMessageChunk(chunk)),
            _ => {}
        }
    }
    updates
}

#[async_trait::async_trait]
impl OupFrontend for Frontend {
    async fn event(&self, event: UiNotification) -> Result<Option<UiCommand>> {
        self.session.peers.event(&event);
        match event {
            UiNotification::EnvelopeV2(event) => self.payload(event.envelope.payload)?,
            UiNotification::ApprovalRequested(event) => {
                use agent_client_protocol::schema::v1::{
                    PermissionOption, PermissionOptionKind, RequestPermissionOutcome,
                    RequestPermissionRequest,
                };
                let request = RequestPermissionRequest::new(
                    self.id.clone(),
                    ToolCallUpdate::new(
                        event.approval_id.0.to_string(),
                        ToolCallUpdateFields::new()
                            .title(event.title)
                            .content(vec![ToolCallContent::from(ContentBlock::from(event.body))]),
                    ),
                    vec![
                        PermissionOption::new(
                            "allow-once",
                            "Allow once",
                            PermissionOptionKind::AllowOnce,
                        ),
                        PermissionOption::new("deny", "Deny", PermissionOptionKind::RejectOnce),
                    ],
                );
                let response = self
                    .cx
                    .send_request(request)
                    .block_task()
                    .await
                    .map_err(|error| eyre::eyre!("ACP permission request failed: {error}"))?;
                let decision = match response.outcome {
                    RequestPermissionOutcome::Selected(selected)
                        if selected.option_id.0.as_ref() == "allow-once" =>
                    {
                        ApprovalDecision::Approve
                    }
                    _ => ApprovalDecision::Deny,
                };
                return Ok(Some(UiCommand::ApprovalRespond(
                    ApprovalRespondParams::new(event.session_id, event.approval_id, decision),
                )));
            }
            _ => {}
        }
        Ok(None)
    }
}

pub(super) async fn serve(
    factory: Arc<dyn SessionAgentFactory>,
    transport: impl agent_client_protocol::ConnectTo<AcpAgentRole> + 'static,
) -> std::result::Result<(), AcpError> {
    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
    serve_with_sessions(factory, sessions, transport).await
}

async fn serve_with_sessions(
    factory: Arc<dyn SessionAgentFactory>,
    sessions: Sessions,
    transport: impl agent_client_protocol::ConnectTo<AcpAgentRole> + 'static,
) -> std::result::Result<(), AcpError> {
    use futures::StreamExt;
    let new_factory = factory.clone();
    let new_sessions = sessions.clone();
    let load_sessions = sessions.clone();
    let prompt_sessions = sessions.clone();
    let cleanup_sessions = sessions.clone();
    let stop = tokio_util::sync::CancellationToken::new();
    let _stop_on_drop = stop.clone().drop_guard();
    let new_stop = stop.clone();
    let load_stop = stop.clone();
    // ACP SDK 1.2's byte transport joins both directions after input EOF.
    // An idle update pump still owns an output sender, so EOF otherwise
    // waits forever. Observe incoming closure independently and tear down
    // this connection's OUP sessions, without imposing a turn deadline.
    let (wire, drive_transport) =
        agent_client_protocol::ConnectTo::<AcpAgentRole>::into_channel_and_future(transport);
    let (agent_transport, bridge) = agent_client_protocol::Channel::duplex();
    let input_stop = stop.clone();
    let incoming = async move {
        let mut incoming = wire.rx;
        while let Some(message) = incoming.next().await {
            bridge.tx.unbounded_send(message).map_err(internal)?;
        }
        input_stop.cancel();
        Ok::<_, AcpError>(())
    };
    let outgoing = async move {
        let mut outgoing = bridge.rx;
        while let Some(message) = outgoing.next().await {
            wire.tx.unbounded_send(message).map_err(internal)?;
        }
        Ok::<_, AcpError>(())
    };
    let io = async {
        futures::try_join!(incoming, outgoing, drive_transport)?;
        Ok::<_, AcpError>(())
    };
    let agent = AcpAgentRole
        .builder()
        .name("octos-acp-oup")
        .on_receive_request(
            async move |req: InitializeRequest, responder, _cx: ConnectionTo<Client>| {
                responder.respond(build_initialize_response(&req))
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |_req: agent_client_protocol::schema::v1::AuthenticateRequest,
                        responder,
                        _cx: ConnectionTo<Client>| {
                responder.respond(agent_client_protocol::schema::v1::AuthenticateResponse::new())
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: NewSessionRequest, responder, cx: ConnectionTo<Client>| {
                let id = new_session_id();
                match open(new_factory.as_ref(), &new_sessions, id.clone(), req.cwd).await {
                    Ok(session) => {
                        start_idle_updates(session, id.clone(), cx, new_stop.clone());
                        responder.respond(NewSessionResponse::new(id))
                    }
                    Err(error) => responder.respond_with_error(error),
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: LoadSessionRequest, responder, cx: ConnectionTo<Client>| {
                let result = async {
                    let session = open(
                        factory.as_ref(),
                        &load_sessions,
                        req.session_id.clone(),
                        req.cwd,
                    )
                    .await?;
                    let history = session.oup.hydrate().await.map_err(internal)?;
                    let frontend = Frontend {
                        id: req.session_id.clone(),
                        cx: cx.clone(),
                        session: session.clone(),
                    };
                    frontend.replay(history).map_err(internal)?;
                    start_idle_updates(session, req.session_id, cx, load_stop.clone());
                    Ok::<_, AcpError>(LoadSessionResponse::new())
                }
                .await;
                match result {
                    Ok(response) => responder.respond(response),
                    Err(error) => responder.respond_with_error(error),
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest,
                        responder: agent_client_protocol::Responder<PromptResponse>,
                        cx: ConnectionTo<Client>| {
                let session = prompt_sessions
                    .lock()
                    .await
                    .get(&req.session_id)
                    .and_then(|slot| slot.get())
                    .cloned();
                let Some(session) = session else {
                    return responder.respond_with_error(internal("unknown session id"));
                };
                if session
                    .busy
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_err()
                {
                    return responder
                        .respond_with_error(internal("session already has an active prompt"));
                }
                // Reset before spawning, and only after atomically accepting the
                // prompt. A competing prompt cannot erase an active cancellation.
                let cancelled = session.cancelled.begin();
                session.busy_watch.send_replace(true);
                let busy = BusyTurn(session.clone());
                cx.clone().spawn(async move {
                    let _busy = busy;
                    let frontend = Frontend {
                        id: req.session_id,
                        cx,
                        session: session.clone(),
                    };
                    let result = session
                        .oup
                        .turn(
                            &extract_prompt_text(&req.prompt),
                            None,
                            &cancelled,
                            &frontend,
                        )
                        .await;
                    match result {
                        Ok(result) => {
                            responder.respond(PromptResponse::new(if result.interrupted {
                                StopReason::Cancelled
                            } else {
                                StopReason::EndTurn
                            }))
                        }
                        Err(error) => responder.respond_with_error(internal(error)),
                    }
                })
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            async move |req: CancelNotification, _cx: ConnectionTo<Client>| {
                let session = sessions
                    .lock()
                    .await
                    .get(&req.session_id)
                    .and_then(|slot| slot.get())
                    .cloned();
                if let Some(session) = session {
                    let cancelled = session.cancelled.current();
                    // Deliver directly, not on a timer after the provider may
                    // already have completed. A pre-start cancel is still carried
                    // by the flag and delivered immediately after turn/start.
                    session.oup.interrupt().await.map_err(internal)?;
                    cancelled.store(true, Ordering::Release);
                }
                Ok(())
            },
            on_receive_notification!(),
        )
        .connect_to(agent_transport);
    let mut result = tokio::select! {
        biased;
        _ = stop.cancelled() => Ok(()),
        result = agent => result,
        result = io => result,
    };
    stop.cancel();
    let sessions: Vec<_> = cleanup_sessions
        .lock()
        .await
        .values()
        .filter_map(|slot| slot.get().cloned())
        .collect();
    for session in sessions {
        session.cancelled.current().store(true, Ordering::Release);
        session.peers.close().await;
        if let Err(error) = session.oup.close().await
            && result.is_ok()
        {
            result = Err(internal(error));
        }
    }
    result
}
