//! Confined ACP execution with one host-owned, memory-only session.

use super::{build_initialize_response, extract_prompt_text, new_session_id, tool_kind_for};
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, ContentChunk, InitializeRequest, InitializeResponse,
    LoadSessionRequest, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    SessionId, SessionNotification, SessionUpdate, StopReason, ToolCall, ToolCallContent,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{
    Agent as AcpAgentRole, Client, ConnectionTo, Error as AcpError, on_receive_notification,
    on_receive_request,
};
use eyre::Result;
use octos_agent::{
    Agent, AgentConfig, ConversationResponse, IncompleteResponseError, ProgressEvent,
    ProgressReporter, Tool, ToolRegistry, ToolResult,
};
use octos_core::{AgentId, Message, MessageRole};
use octos_llm::host::{
    self, HostConfig, ModelRequest, ToolCallRequest, ToolCallResponse, ToolsListResponse,
};
use octos_llm::{ChatConfig, ChatResponse, LlmProvider, ToolSpec};
use octos_memory::EpisodeStore;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::AtomicU64;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::Mutex;
use tokio::sync::Notify;

/// Host-managed history has no disk store or workspace identity by construction.
struct ManagedSession {
    agent: Arc<Agent>,
    history: Mutex<Vec<Message>>,
    shutdown: Arc<AtomicBool>,
    fold_queue: Mutex<Vec<String>>,
}

type SessionMap = Arc<Mutex<HashMap<SessionId, Arc<ManagedSession>>>>;
const FOLD_QUEUE_MAX_EVENTS: usize = 16;
const DEFAULT_HOST_MAX_ITERATIONS: u32 = 20;
const AUTO_RESPOND_INSTRUCTION: &str = "Host notifications were just delivered to you (the [Notification] rows \
    in your context). Review them and act if appropriate — continue a task \
    whose prerequisite you were told just finished, or comment on newly \
    available information. These notifications are NOT user messages: do \
    not answer them as if a user spoke, and do not fabricate or paraphrase \
    a user request. If nothing warrants action, reply with no text at all.";

/// What a `session/notify` request does when the session has a turn in
/// flight (`busy`). Wire values: `"drop"` (default) discards the events;
/// `"fold"` queues them (bounded, oldest first) and injects them before the
/// next turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyIfBusy {
    /// Discard the events: a busy agent is mid-turn on older context, and
    /// stale notifications are worse than none.
    #[default]
    Drop,
    /// Queue the events (bounded) and inject them before the next turn.
    Fold,
}

/// Client→agent `session/notify` request: push host event/notification
/// context into a live session WITHOUT it being (or being mistakable for) a
/// user turn.
///
/// Not part of the ACP spec — a host-managed extension for embedding hosts (Robrix
/// AI Rooms) that need to tell an agent about room events. Registered via the
/// `agent-client-protocol` request derives so the JSON-RPC runtime dispatches
/// it exactly like a spec request.
///
/// Wire shape (snake_case, matching the embedding contract):
/// ```text
/// { "session_id": "…", "events": ["…"],
///   "auto_respond": false, "if_busy": "drop" }
/// ```
/// `sessionId` is also accepted as an alias for ACP-convention clients.
#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcRequest)]
#[request(method = "session/notify", response = NotifyResponse)]
#[serde(rename_all = "snake_case")]
pub struct NotifyRequest {
    /// The session to notify.
    #[serde(alias = "sessionId")]
    pub session_id: SessionId,
    /// One or more plain-text event descriptions, already human-readable
    /// ("Message edited in Design chat", "read_room_messages was denied: …").
    pub events: Vec<String>,
    /// `false` (default): context only — the agent does nothing until the
    /// next user prompt. `true`: on an idle session, start a turn exactly as
    /// if a prompt had arrived, with the events as context.
    #[serde(default)]
    pub auto_respond: bool,
    /// What to do when a turn is in flight. `"drop"` (default) or `"fold"`.
    #[serde(default)]
    pub if_busy: NotifyIfBusy,
}

/// Acknowledgment for [`NotifyRequest`]. Sent immediately, before any LLM
/// work; `queued` is `false` only when the events were discarded
/// (`busy` + `if_busy: drop`).
#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcResponse)]
#[serde(rename_all = "snake_case")]
pub struct NotifyResponse {
    /// Whether the events were accepted (stored now, or queued for the next
    /// turn). `false` means they were discarded under `if_busy: drop`.
    pub queued: bool,
    /// Whether a turn was in flight when the request was handled.
    pub busy: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcRequest)]
#[request(method = "_octos/host/model", response = ModelResponse)]
#[serde(transparent)]
struct ModelCall(ModelRequest);

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcResponse)]
#[serde(transparent)]
struct ModelResponse(ChatResponse);

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcRequest)]
#[request(method = "_octos/host/tools/list", response = ToolsResponse)]
#[serde(transparent)]
struct ListTools(host::ToolsListRequest);

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcResponse)]
#[serde(transparent)]
struct ToolsResponse(ToolsListResponse);

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcRequest)]
#[request(method = "_octos/host/tools/call", response = ToolResponse)]
#[serde(transparent)]
struct CallTool(ToolCallRequest);

#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcResponse)]
#[serde(transparent)]
struct ToolResponse(ToolCallResponse);

#[derive(Default)]
struct Lifetime {
    shutdown: Arc<AtomicBool>,
    generation: AtomicU64,
    cancelled: Notify,
}

impl Lifetime {
    fn cancel(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.cancelled.notify_waiters();
    }

    fn check(&self, generation: u64) -> Result<()> {
        eyre::ensure!(
            !self.shutdown.load(Ordering::Acquire)
                && self.generation.load(Ordering::Acquire) == generation,
            "host-managed request cancelled"
        );
        Ok(())
    }

    async fn wait_for_cancel(&self, generation: u64) {
        loop {
            let notified = self.cancelled.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.check(generation).is_err() {
                return;
            }
            notified.await;
        }
    }
}

#[derive(Clone)]
struct Broker {
    connection: ConnectionTo<Client>,
    lifetime: Arc<Lifetime>,
    generation: u64,
}

impl Broker {
    async fn request<R: agent_client_protocol::JsonRpcRequest>(
        &self,
        request: R,
    ) -> Result<R::Response> {
        let generation = self.generation;
        self.lifetime.check(generation)?;
        let result = tokio::select! {
            biased;
            _ = self.lifetime.wait_for_cancel(generation) => eyre::bail!("host-managed request cancelled"),
            result = self.connection.send_request(request).block_task() => result,
        };
        self.lifetime.check(generation)?;
        result.map_err(|_| eyre::eyre!("host broker request failed or disconnected"))
    }

    async fn tools(&self) -> Result<Vec<ToolSpec>> {
        let response = self.request(ListTools(host::ToolsListRequest {})).await?.0;
        response.validate().map_err(eyre::Report::msg)?;
        Ok(response.tools)
    }
}

struct HostProvider {
    broker: Broker,
    model: host::HostModel,
}

#[async_trait::async_trait]
impl LlmProvider for HostProvider {
    async fn chat(
        &self,
        messages: &[octos_core::Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<ChatResponse> {
        let request = ModelRequest {
            messages: messages.to_vec(),
            tools: tools.to_vec(),
            config: config.clone(),
        };
        request.validate().map_err(eyre::Report::msg)?;
        let response = self.broker.request(ModelCall(request)).await?.0;
        host::validate_payload_size(&response).map_err(eyre::Report::msg)?;
        Ok(response)
    }

    async fn chat_stream(
        &self,
        messages: &[octos_core::Message],
        tools: &[ToolSpec],
        config: &ChatConfig,
    ) -> Result<octos_llm::ChatStream> {
        Ok(host::response_stream(
            self.chat(messages, tools, config).await?,
        ))
    }

    fn model_id(&self) -> &str {
        &self.model.model_id
    }
    fn provider_name(&self) -> &str {
        &self.model.provider_name
    }
    fn context_window(&self) -> u32 {
        self.model.context_window
    }
    fn max_output_tokens(&self) -> u32 {
        self.model.max_output_tokens
    }
}

struct HostTool {
    spec: ToolSpec,
    broker: Broker,
}

#[async_trait::async_trait]
impl Tool for HostTool {
    fn name(&self) -> &str {
        &self.spec.name
    }
    fn description(&self) -> &str {
        &self.spec.description
    }
    fn input_schema(&self) -> serde_json::Value {
        self.spec.input_schema.clone()
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let request = ToolCallRequest {
            name: self.spec.name.clone(),
            arguments: args.clone(),
        };
        host::validate_payload_size(&request).map_err(eyre::Report::msg)?;
        let response = self.broker.request(CallTool(request)).await?.0;
        host::validate_payload_size(&response).map_err(eyre::Report::msg)?;
        Ok(ToolResult {
            output: response.content,
            success: !response.is_error,
            ..Default::default()
        })
    }
}

struct ManagedState {
    config: OnceLock<HostConfig>,
    session_created: AtomicBool,
    turn_started: AtomicBool,
    agent_generation: AtomicU64,
    lifetime: Arc<Lifetime>,
    sessions: SessionMap,
    session_update: Mutex<()>,
    max_iterations: u32,
}

impl ManagedState {
    fn new(max_iterations: u32) -> Self {
        Self {
            config: OnceLock::new(),
            session_created: AtomicBool::new(false),
            turn_started: AtomicBool::new(false),
            lifetime: Arc::new(Lifetime::default()),
            agent_generation: AtomicU64::new(0),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            max_iterations: if max_iterations == 0 {
                DEFAULT_HOST_MAX_ITERATIONS
            } else {
                max_iterations
            },
            session_update: Mutex::new(()),
        }
    }

    fn initialize(&self, request: &InitializeRequest, sandbox: &str) -> Result<InitializeResponse> {
        let value = request
            .client_capabilities
            .meta
            .as_ref()
            .and_then(|meta| meta.get(host::CAPABILITY_KEY))
            .ok_or_else(|| eyre::eyre!("host-managed broker capability is required"))?;
        let config: HostConfig = serde_json::from_value(value.clone())?;
        config.validate().map_err(eyre::Report::msg)?;
        self.config
            .set(config)
            .map_err(|_| eyre::eyre!("already initialized"))?;
        let mut response = build_initialize_response(request);
        response.agent_capabilities.load_session = false;
        response.agent_capabilities.meta = Some(
            [(
                host::CAPABILITY_KEY.to_string(),
                serde_json::to_value(host::HostCapabilities {
                    version: host::VERSION,
                    confined: true,
                    sandbox: sandbox.into(),
                })?,
            )]
            .into_iter()
            .collect(),
        );
        Ok(response)
    }

    fn agent(
        &self,
        broker: Broker,
        specs: Vec<ToolSpec>,
        memory: Arc<EpisodeStore>,
    ) -> Result<Arc<Agent>> {
        let config = self
            .config
            .get()
            .ok_or_else(|| eyre::eyre!("initialize is required"))?;
        let provider = Arc::new(HostProvider {
            broker: broker.clone(),
            model: config.model.clone(),
        });
        let mut tools = ToolRegistry::new();
        for spec in specs {
            tools.register(HostTool {
                spec,
                broker: broker.clone(),
            });
        }
        let agent_config = AgentConfig {
            max_iterations: self.max_iterations,
            chat_max_tokens: Some(config.model.max_output_tokens),
            save_episodes: false,
            suppress_auto_send_files: true,
            format_after_edit: false,
            ..Default::default()
        };
        Ok(Arc::new(
            Agent::new(AgentId::new("host-managed"), provider, tools, memory)
                .with_config(agent_config)
                .with_shutdown(self.lifetime.shutdown.clone())
                .with_system_prompt(config.system_prompt.clone()),
        ))
    }

    async fn new_session(
        &self,
        request: NewSessionRequest,
        connection: ConnectionTo<Client>,
    ) -> Result<NewSessionResponse> {
        eyre::ensure!(self.config.get().is_some(), "initialize is required");
        eyre::ensure!(
            request.mcp_servers.is_empty(),
            "host-managed sessions reject external MCP servers"
        );
        eyre::ensure!(
            self.session_created
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "host-managed processes allow exactly one session"
        );
        let broker = Broker {
            connection,
            lifetime: self.lifetime.clone(),
            generation: 0,
        };
        let agent = self.agent(broker, Vec::new(), Arc::new(EpisodeStore::in_memory()?))?;
        let session_id = new_session_id();
        let session = Arc::new(ManagedSession {
            agent,
            history: Mutex::new(Vec::new()),
            shutdown: self.lifetime.shutdown.clone(),
            fold_queue: Mutex::new(Vec::new()),
        });
        self.sessions
            .lock()
            .await
            .insert(session_id.clone(), session);
        Ok(NewSessionResponse::new(session_id))
    }

    async fn session(&self, id: &SessionId) -> Result<Arc<ManagedSession>> {
        self.sessions
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| eyre::eyre!("unknown host-managed session"))
    }

    /// Refresh the host's tool snapshot between turns. The immutable Agent
    /// registry is replaced only while idle; history and the in-memory store
    /// remain in this one session. Every call still rechecks the host registry.
    async fn refresh(
        &self,
        id: &SessionId,
        connection: ConnectionTo<Client>,
        generation: u64,
    ) -> Result<Arc<ManagedSession>> {
        let broker = Broker {
            connection,
            lifetime: self.lifetime.clone(),
            generation,
        };
        let mut specs = broker.tools().await?;
        self.lifetime.check(generation)?;
        let _update = self.session_update.lock().await;
        let old = self.session(id).await?;
        let mut current = old.agent.tool_registry().specs();
        specs.sort_by(|left, right| left.name.cmp(&right.name));
        current.sort_by(|left, right| left.name.cmp(&right.name));
        if self.agent_generation.load(Ordering::Acquire) == generation
            && current.len() == specs.len()
            && current.iter().zip(&specs).all(|(left, right)| {
                left.name == right.name
                    && left.description == right.description
                    && left.input_schema == right.input_schema
            })
        {
            return Ok(old);
        }
        let agent = self.agent(broker, specs, old.agent.memory_store().clone())?;
        let session = Arc::new(ManagedSession {
            agent,
            history: Mutex::new(std::mem::take(&mut *old.history.lock().await)),
            shutdown: self.lifetime.shutdown.clone(),
            fold_queue: Mutex::new(std::mem::take(&mut *old.fold_queue.lock().await)),
        });
        self.sessions
            .lock()
            .await
            .insert(id.clone(), session.clone());
        self.agent_generation.store(generation, Ordering::Release);
        Ok(session)
    }

    fn begin_turn(self: &Arc<Self>) -> Result<TurnGuard> {
        eyre::ensure!(
            self.turn_started
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "a host-managed turn is already running"
        );
        self.lifetime.shutdown.store(false, Ordering::Release);
        Ok(TurnGuard(self.clone()))
    }
}

struct TurnGuard(Arc<ManagedState>);

impl Drop for TurnGuard {
    fn drop(&mut self) {
        self.0.turn_started.store(false, Ordering::Release);
    }
}

fn error(error: impl std::fmt::Display) -> AcpError {
    agent_client_protocol::util::internal_error(error.to_string())
}

/// Call only after OS confinement; tests pass an in-process transport and
/// exercise the same protocol without changing their own process sandbox.
pub(super) async fn serve(
    max_iterations: u32,
    sandbox: &'static str,
    transport: impl agent_client_protocol::ConnectTo<AcpAgentRole> + 'static,
) -> std::result::Result<(), AcpError> {
    let state = Arc::new(ManagedState::new(max_iterations));
    let init = state.clone();
    let new = state.clone();
    let prompt = state.clone();
    let notify = state.clone();
    let cancel = state.clone();
    let result = AcpAgentRole
        .builder()
        .name("octos-host-managed")
        .on_receive_request(
            async move |req: InitializeRequest, responder, _cx: ConnectionTo<Client>| match init
                .initialize(&req, sandbox)
            {
                Ok(response) => responder.respond(response),
                Err(e) => responder.respond_with_error(error(e)),
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: NewSessionRequest, responder, cx: ConnectionTo<Client>| match new
                .new_session(req, cx)
                .await
            {
                Ok(response) => responder.respond(response),
                Err(e) => responder.respond_with_error(error(e)),
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |_req: LoadSessionRequest, responder, _cx: ConnectionTo<Client>| {
                responder.respond_with_error(error(
                    "host-managed sessions cannot load persistent history",
                ))
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest,
                        responder: agent_client_protocol::Responder<PromptResponse>,
                        cx: ConnectionTo<Client>| {
                if let Err(e) = host::validate_payload_size(&req) {
                    return responder.respond_with_error(error(e));
                }
                if let Err(e) = prompt.session(&req.session_id).await {
                    return responder.respond_with_error(error(e));
                }
                let guard = match prompt.begin_turn() {
                    Ok(guard) => guard,
                    Err(e) => return responder.respond_with_error(error(e)),
                };
                let generation = prompt.lifetime.generation.load(Ordering::Acquire);
                let state = prompt.clone();
                cx.clone().spawn(async move {
                    let _guard = guard;
                    match state.refresh(&req.session_id, cx.clone(), generation).await {
                        Ok(session) => run_prompt_turn(session, req, cx, responder).await,
                        Err(e) => {
                            if state.lifetime.check(generation).is_err() {
                                responder.respond(PromptResponse::new(StopReason::Cancelled))
                            } else {
                                responder.respond_with_error(error(e))
                            }
                        }
                    }
                })
            },
            on_receive_request!(),
        )
        .on_receive_request(
            async move |req: NotifyRequest,
                        responder: agent_client_protocol::Responder<NotifyResponse>,
                        cx: ConnectionTo<Client>| {
                if let Err(e) = host::validate_payload_size(&req) {
                    return responder.respond_with_error(error(e));
                }
                if req.events.is_empty() {
                    return responder
                        .respond_with_error(error("host notification events must not be empty"));
                }
                let _update = notify.session_update.lock().await;
                let session = match notify.session(&req.session_id).await {
                    Ok(session) => session,
                    Err(e) => return responder.respond_with_error(error(e)),
                };
                let busy = notify.turn_started.load(Ordering::Acquire);
                if !req.auto_respond || busy {
                    // The turn guard covers tool refresh as well as inference.
                    let response = notify_context_only(&session, &req, busy).await;
                    return responder.respond(response);
                }
                let guard = match notify.begin_turn() {
                    Ok(guard) => guard,
                    // A prompt may have started since the idle snapshot. Apply
                    // the caller's busy policy without starting another turn.
                    Err(_) => {
                        return responder.respond(notify_context_only(&session, &req, true).await);
                    }
                };
                let folded = drain_fold_events(&session).await;
                append_host_events(&session, &folded).await;
                append_host_events(&session, &req.events).await;
                let generation = notify.lifetime.generation.load(Ordering::Acquire);
                let state = notify.clone();
                responder.respond(NotifyResponse {
                    queued: true,
                    busy: false,
                })?;
                cx.clone().spawn(async move {
                    let _guard = guard;
                    if let Ok(session) =
                        state.refresh(&req.session_id, cx.clone(), generation).await
                    {
                        let _ = run_turn_core(
                            &session,
                            &req.session_id,
                            AUTO_RESPOND_INSTRUCTION,
                            true,
                            &cx,
                        )
                        .await;
                    }
                    Ok(())
                })
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            async move |req: CancelNotification, _cx: ConnectionTo<Client>| {
                if cancel.session(&req.session_id).await.is_ok() {
                    cancel.lifetime.cancel();
                }
                Ok(())
            },
            on_receive_notification!(),
        )
        .connect_to(transport)
        .await;
    state.lifetime.cancel();
    result
}

async fn notify_context_only(
    session: &ManagedSession,
    req: &NotifyRequest,
    busy: bool,
) -> NotifyResponse {
    if busy {
        let queued = req.if_busy == NotifyIfBusy::Fold;
        if queued {
            let mut queue = session.fold_queue.lock().await;
            queue.extend(req.events.iter().cloned());
            let excess = queue.len().saturating_sub(FOLD_QUEUE_MAX_EVENTS);
            queue.drain(..excess);
        }
        return NotifyResponse { queued, busy: true };
    }
    let folded = drain_fold_events(session).await;
    append_host_events(session, &folded).await;
    append_host_events(session, &req.events).await;
    NotifyResponse {
        queued: true,
        busy: false,
    }
}

async fn drain_fold_events(session: &ManagedSession) -> Vec<String> {
    std::mem::take(&mut *session.fold_queue.lock().await)
}

async fn append_host_events(session: &ManagedSession, events: &[String]) {
    session.history.lock().await.extend(
        events
            .iter()
            .map(|event| Message::system(format!("[Notification] {event}"))),
    );
}

enum TurnEnd {
    Done(StopReason),
    Failed(String),
}

async fn run_prompt_turn(
    session: Arc<ManagedSession>,
    req: PromptRequest,
    cx: ConnectionTo<Client>,
    responder: agent_client_protocol::Responder<PromptResponse>,
) -> std::result::Result<(), AcpError> {
    let end = run_turn_core(
        &session,
        &req.session_id,
        &extract_prompt_text(&req.prompt),
        false,
        &cx,
    )
    .await;
    match end {
        TurnEnd::Done(reason) => responder.respond(PromptResponse::new(reason)),
        TurnEnd::Failed(message) => responder.respond_with_error(error(message)),
    }
}

/// Run the canonical Agent; only this adapter's session state stays in memory.
async fn run_turn_core(
    session: &ManagedSession,
    session_id: &SessionId,
    user_text: &str,
    synthetic_user_row: bool,
    cx: &ConnectionTo<Client>,
) -> TurnEnd {
    let folded = drain_fold_events(session).await;
    append_host_events(session, &folded).await;
    let history = session.history.lock().await.clone();
    session
        .agent
        .set_reporter(Arc::new(AcpProgressReporter::new(
            session_id.clone(),
            cx.clone(),
        )));
    let outcome = session
        .agent
        .process_message(user_text, &history, vec![])
        .await;
    let cancelled = session.shutdown.load(Ordering::Acquire);
    match outcome {
        Ok(response) => {
            retain_turn_rows(session, response, cancelled, synthetic_user_row).await;
            TurnEnd::Done(if cancelled {
                StopReason::Cancelled
            } else {
                StopReason::EndTurn
            })
        }
        Err(err) => {
            // Upstream carries real partial output in a typed error. Preserve
            // that context without reporting a truncated response as success.
            if let Some(incomplete) = err.downcast_ref::<IncompleteResponseError>() {
                retain_turn_rows(
                    session,
                    incomplete.partial.clone(),
                    cancelled,
                    synthetic_user_row,
                )
                .await;
            }
            if cancelled {
                TurnEnd::Done(StopReason::Cancelled)
            } else {
                TurnEnd::Failed(format!("prompt turn failed: {err}"))
            }
        }
    }
}

/// Append this turn's rows, excluding the synthetic notification instruction.
async fn retain_turn_rows(
    session: &ManagedSession,
    response: ConversationResponse,
    cancelled: bool,
    synthetic_user_row: bool,
) {
    let mut rows = response.messages;
    if synthetic_user_row {
        rows.retain(|message| message.role != MessageRole::User);
    }
    let mut history = session.history.lock().await;
    history.extend(rows);
    let already_retained = history.last().is_some_and(|message| {
        message.role == MessageRole::Assistant && message.content == response.content
    });
    if !cancelled && !response.content.is_empty() && !already_retained {
        let mut message = Message::assistant(response.content);
        message.reasoning_content = response.reasoning_content;
        history.push(message);
    }
}

fn progress_event_to_acp(event: &ProgressEvent) -> Option<SessionUpdate> {
    match event {
        ProgressEvent::StreamChunk { text, .. } => Some(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::from(text.clone())),
        )),
        ProgressEvent::Response { content, .. } => Some(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::from(content.clone())),
        )),
        ProgressEvent::ReasoningChunk { text, .. } => Some(SessionUpdate::AgentThoughtChunk(
            ContentChunk::new(ContentBlock::from(text.clone())),
        )),
        ProgressEvent::ToolStarted { name, tool_id, .. } => {
            let call = ToolCall::new(tool_id.clone(), name.clone())
                .kind(tool_kind_for(name))
                .status(ToolCallStatus::InProgress);
            Some(SessionUpdate::ToolCall(call))
        }
        ProgressEvent::ToolCompleted {
            tool_id,
            success,
            output_preview,
            ..
        } => {
            let status = if *success {
                ToolCallStatus::Completed
            } else {
                ToolCallStatus::Failed
            };
            let mut fields = ToolCallUpdateFields::new().status(status);
            if !output_preview.is_empty() {
                let content: ToolCallContent =
                    ToolCallContent::from(ContentBlock::from(output_preview.clone()));
                fields = fields.content(vec![content]);
            }
            Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                tool_id.clone(),
                fields,
            )))
        }
        // No ACP projection: lifecycle/bookkeeping/token/cost events.
        _ => None,
    }
}

struct AcpProgressReporter {
    session_id: SessionId,
    cx: ConnectionTo<Client>,
    // Store iteration + 1 so zero represents no streamed iteration.
    streamed_iteration: AtomicU64,
}

impl AcpProgressReporter {
    fn new(session_id: SessionId, cx: ConnectionTo<Client>) -> Self {
        Self {
            session_id,
            cx,
            streamed_iteration: AtomicU64::new(0),
        }
    }
}

impl ProgressReporter for AcpProgressReporter {
    fn report(&self, event: ProgressEvent) {
        if let Some(update) = project_progress_event(&event, &self.streamed_iteration) {
            let notification = SessionNotification::new(self.session_id.clone(), update);
            if self.cx.send_notification(notification).is_err() {
                tracing::warn!("failed to send host-managed ACP session/update");
            }
        }
    }
}

fn project_progress_event(
    event: &ProgressEvent,
    streamed_iteration: &AtomicU64,
) -> Option<SessionUpdate> {
    match event {
        ProgressEvent::StreamChunk { iteration, .. } => {
            streamed_iteration.store(u64::from(*iteration) + 1, Ordering::Relaxed);
        }
        ProgressEvent::Response { iteration, .. }
            if streamed_iteration.load(Ordering::Relaxed) == u64::from(*iteration) + 1 =>
        {
            return None;
        }
        _ => {}
    }
    progress_event_to_acp(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::ProtocolVersion;
    use agent_client_protocol::schema::v1::{ClientCapabilities, TextContent};
    use octos_core::MessageRole;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicUsize;

    struct Transport;

    impl agent_client_protocol::ConnectTo<Client> for Transport {
        async fn connect_to(
            self,
            client: impl agent_client_protocol::ConnectTo<AcpAgentRole> + 'static,
        ) -> std::result::Result<(), AcpError> {
            serve(4, "test-only", client).await
        }
    }

    fn initialize() -> InitializeRequest {
        let config = HostConfig {
            version: host::VERSION,
            model: host::HostModel {
                model_id: "host-test".into(),
                provider_name: "broker".into(),
                context_window: 32_000,
                max_output_tokens: 1024,
            },
            system_prompt: "Use only tools supplied by the host.".into(),
        };
        let mut capabilities = ClientCapabilities::new();
        capabilities.meta = Some(
            [(
                host::CAPABILITY_KEY.into(),
                serde_json::to_value(config).unwrap(),
            )]
            .into_iter()
            .collect(),
        );
        InitializeRequest::new(ProtocolVersion::V1).client_capabilities(capabilities)
    }

    fn prompt(session: SessionId, text: &str) -> PromptRequest {
        PromptRequest::new(session, vec![ContentBlock::Text(TextContent::new(text))])
    }

    fn response(content: &str) -> ChatResponse {
        ChatResponse {
            content: Some(content.into()),
            reasoning_content: None,
            tool_calls: Vec::new(),
            stop_reason: octos_llm::StopReason::EndTurn,
            usage: Default::default(),
            provider_index: None,
        }
    }

    fn tool(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.into(),
            description: "Host operation".into(),
            input_schema: serde_json::json!({"type":"object"}),
        }
    }

    #[test]
    fn host_managed_default_turn_limit_stays_finite() {
        assert_eq!(ManagedState::new(0).max_iterations, 20);
        assert_eq!(ManagedState::new(7).max_iterations, 7);
        assert_eq!(super::super::AcpCommand::default().max_iterations, 0);
    }

    #[test]
    fn host_managed_old_generation_stays_cancelled_after_next_turn_starts() {
        let lifetime = Lifetime::default();
        assert!(lifetime.check(0).is_ok());
        lifetime.cancel();
        lifetime.shutdown.store(false, Ordering::Release);
        assert!(lifetime.check(0).is_err());
        assert!(lifetime.check(1).is_ok());
        #[cfg(feature = "api")]
        {
            let command = super::super::AcpCommand {
                host_managed: true,
                config: Some("/never-read-this-config".into()),
                ..Default::default()
            };
            assert!(
                command
                    .factory()
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("confined ACP broker transport")
            );
        }
    }

    #[tokio::test]
    async fn host_managed_requires_negotiation_and_refuses_history_and_extra_sessions() {
        Client.builder().connect_with(Transport, |cx: ConnectionTo<AcpAgentRole>| async move {
            assert!(cx.send_request(InitializeRequest::new(ProtocolVersion::V1)).block_task().await.is_err());
            assert!(cx.send_request(NewSessionRequest::new(PathBuf::from("/"))).block_task().await.is_err());
            let initialized = cx.send_request(initialize()).block_task().await?;
            assert!(!initialized.agent_capabilities.load_session);
            let caps = &initialized.agent_capabilities.meta.as_ref().unwrap()[host::CAPABILITY_KEY];
            assert_eq!(caps["version"], host::VERSION);
            assert_eq!(caps["confined"], true);
            assert!(cx.send_request(initialize()).block_task().await.is_err());
            let with_mcp: NewSessionRequest = serde_json::from_value(serde_json::json!({
                "cwd":"/", "mcpServers":[{"name":"bypass", "command":"/bin/sh", "args":[], "env":[]}]
            })).unwrap();
            assert!(cx.send_request(with_mcp).block_task().await.is_err());
            let session = cx.send_request(NewSessionRequest::new(PathBuf::from("/"))).block_task().await?.session_id;
            assert!(cx.send_request(LoadSessionRequest::new(session, PathBuf::from("/"))).block_task().await.is_err());
            assert!(cx.send_request(NewSessionRequest::new(PathBuf::from("/"))).block_task().await.is_err());
            Ok(())
        }).await.unwrap();
    }

    #[tokio::test]
    async fn host_managed_missing_broker_fails_before_inference() {
        Client
            .builder()
            .connect_with(Transport, |cx: ConnectionTo<AcpAgentRole>| async move {
                cx.send_request(initialize()).block_task().await?;
                let session = cx
                    .send_request(NewSessionRequest::new(PathBuf::from("/")))
                    .block_task()
                    .await?
                    .session_id;
                assert!(
                    cx.send_request(prompt(session, "private data"))
                        .block_task()
                        .await
                        .is_err()
                );
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn host_managed_routes_model_and_tools_and_refreshes_tool_snapshot() {
        let model_calls = Arc::new(AtomicUsize::new(0));
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let lists = Arc::new(AtomicUsize::new(0));
        let model_count = model_calls.clone();
        let tool_count = tool_calls.clone();
        let list_count = lists.clone();
        Client
            .builder()
            .on_receive_notification(
                async |_req: SessionNotification, _cx: ConnectionTo<AcpAgentRole>| Ok(()),
                on_receive_notification!(),
            )
            .on_receive_request(
                async move |_req: ListTools, responder, _cx: ConnectionTo<AcpAgentRole>| {
                    let name = if list_count.fetch_add(1, Ordering::AcqRel) == 0 {
                        "echo"
                    } else {
                        "changed"
                    };
                    responder.respond(ToolsResponse(ToolsListResponse {
                        tools: vec![tool(name)],
                    }))
                },
                on_receive_request!(),
            )
            .on_receive_request(
                async move |req: CallTool, responder, _cx: ConnectionTo<AcpAgentRole>| {
                    assert_eq!(req.0.name, "echo");
                    assert_eq!(req.0.arguments["text"], "private tool input");
                    tool_count.fetch_add(1, Ordering::AcqRel);
                    responder.respond(ToolResponse(ToolCallResponse {
                        content: "private tool feedback".into(),
                        is_error: false,
                    }))
                },
                on_receive_request!(),
            )
            .on_receive_request(
                async move |req: ModelCall, responder, _cx: ConnectionTo<AcpAgentRole>| {
                    let index = model_count.fetch_add(1, Ordering::AcqRel);
                    assert_eq!(
                        req.0.tools.len(),
                        1,
                        "native Octos tools must not be registered"
                    );
                    assert_eq!(req.0.config.max_tokens, Some(1024));
                    let mut result = response("done");
                    if index == 0 {
                        assert_eq!(req.0.tools[0].name, "echo");
                        result.tool_calls.push(octos_core::ToolCall {
                            id: "call-1".into(),
                            name: "echo".into(),
                            arguments: serde_json::json!({"text":"private tool input"}),
                            metadata: None,
                        });
                        result.stop_reason = octos_llm::StopReason::ToolUse;
                    } else if index == 1 {
                        assert!(
                            req.0
                                .messages
                                .iter()
                                .any(|message| message.role == MessageRole::Tool
                                    && message.content.contains("private tool feedback"))
                        );
                    } else {
                        assert_eq!(req.0.tools[0].name, "changed");
                        assert!(
                            req.0
                                .messages
                                .iter()
                                .any(|message| message.content.contains("private tool feedback")),
                            "tool refresh must preserve history"
                        );
                    }
                    responder.respond(ModelResponse(result))
                },
                on_receive_request!(),
            )
            .connect_with(Transport, |cx: ConnectionTo<AcpAgentRole>| async move {
                cx.send_request(initialize()).block_task().await?;
                let session = cx
                    .send_request(NewSessionRequest::new(PathBuf::from("/")))
                    .block_task()
                    .await?
                    .session_id;
                assert_eq!(
                    cx.send_request(prompt(session.clone(), "first"))
                        .block_task()
                        .await?
                        .stop_reason,
                    StopReason::EndTurn
                );
                assert_eq!(
                    cx.send_request(prompt(session, "second"))
                        .block_task()
                        .await?
                        .stop_reason,
                    StopReason::EndTurn
                );
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(model_calls.load(Ordering::Acquire), 3);
        assert_eq!(tool_calls.load(Ordering::Acquire), 1);
        assert_eq!(lists.load(Ordering::Acquire), 2);
    }

    #[tokio::test]
    async fn host_managed_cancel_interrupts_broker_wait_and_allows_a_fresh_turn() {
        let entered = Arc::new(Notify::new());
        let entered_model = entered.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let model_calls = calls.clone();
        let pending = Arc::new(Mutex::new(None));
        let pending_model = pending.clone();
        Client
            .builder()
            .on_receive_notification(
                async |_req: SessionNotification, _cx: ConnectionTo<AcpAgentRole>| Ok(()),
                on_receive_notification!(),
            )
            .on_receive_request(
                async |_req: ListTools, responder, _cx: ConnectionTo<AcpAgentRole>| {
                    responder.respond(ToolsResponse(ToolsListResponse { tools: Vec::new() }))
                },
                on_receive_request!(),
            )
            .on_receive_request(
                async move |_req: ModelCall, responder, _cx: ConnectionTo<AcpAgentRole>| {
                    if model_calls.fetch_add(1, Ordering::AcqRel) == 0 {
                        *pending_model.lock().await = Some(responder);
                        entered_model.notify_one();
                        Ok(())
                    } else {
                        responder.respond(ModelResponse(response("fresh")))
                    }
                },
                on_receive_request!(),
            )
            .connect_with(Transport, |cx: ConnectionTo<AcpAgentRole>| async move {
                cx.send_request(initialize()).block_task().await?;
                let session = cx
                    .send_request(NewSessionRequest::new(PathBuf::from("/")))
                    .block_task()
                    .await?
                    .session_id;
                let canceller = cx.clone();
                let cancel_session = session.clone();
                let task = tokio::spawn(async move {
                    entered.notified().await;
                    assert!(
                        canceller
                            .send_request(prompt(cancel_session.clone(), "overlapping turn"))
                            .block_task()
                            .await
                            .is_err()
                    );
                    canceller
                        .send_notification(CancelNotification::new(cancel_session))
                        .unwrap();
                });
                let first = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    cx.send_request(prompt(session.clone(), "cancel this"))
                        .block_task(),
                )
                .await
                .unwrap()?;
                assert_eq!(first.stop_reason, StopReason::Cancelled);
                task.await.unwrap();
                assert_eq!(
                    cx.send_request(prompt(session, "fresh turn"))
                        .block_task()
                        .await?
                        .stop_reason,
                    StopReason::EndTurn
                );
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(
            calls.load(Ordering::Acquire),
            2,
            "cancelled calls must not fall back to a second broker request"
        );
    }

    #[test]
    fn host_managed_progress_deduplicates_each_iteration_independently() {
        let streamed = AtomicU64::new(0);
        assert!(
            project_progress_event(
                &ProgressEvent::Response {
                    content: "plain".into(),
                    iteration: 0
                },
                &streamed
            )
            .is_some()
        );
        assert!(
            project_progress_event(
                &ProgressEvent::StreamChunk {
                    text: "streamed".into(),
                    iteration: 1
                },
                &streamed
            )
            .is_some()
        );
        assert!(
            project_progress_event(
                &ProgressEvent::Response {
                    content: "streamed".into(),
                    iteration: 1
                },
                &streamed
            )
            .is_none()
        );
        assert!(
            project_progress_event(
                &ProgressEvent::Response {
                    content: "next answer".into(),
                    iteration: 2
                },
                &streamed
            )
            .is_some()
        );
        assert!(matches!(
            progress_event_to_acp(&ProgressEvent::ReasoningChunk {
                text: "reasoning".into(),
                iteration: 2
            }),
            Some(SessionUpdate::AgentThoughtChunk(_))
        ));
        assert!(matches!(
            progress_event_to_acp(&ProgressEvent::ToolStarted {
                name: "host-tool".into(),
                tool_id: "call-1".into(),
                arguments: None
            }),
            Some(SessionUpdate::ToolCall(_))
        ));
        assert!(matches!(
            progress_event_to_acp(&ProgressEvent::ToolCompleted {
                name: "host-tool".into(),
                tool_id: "call-1".into(),
                success: false,
                output_preview: "denied".into(),
                duration: std::time::Duration::ZERO,
            }),
            Some(SessionUpdate::ToolCallUpdate(_))
        ));
    }

    #[tokio::test]
    async fn host_managed_notifications_fold_bounded_context_without_starting_another_turn() {
        let entered = Arc::new(Notify::new());
        let model_entered = entered.clone();
        let pending = Arc::new(Mutex::new(None));
        let model_pending = pending.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let model_calls = calls.clone();
        Client
            .builder()
            .on_receive_notification(
                async |_req: SessionNotification, _cx: ConnectionTo<AcpAgentRole>| Ok(()),
                on_receive_notification!(),
            )
            .on_receive_request(
                async |_req: ListTools, responder, _cx: ConnectionTo<AcpAgentRole>| {
                    responder.respond(ToolsResponse(ToolsListResponse { tools: Vec::new() }))
                },
                on_receive_request!(),
            )
            .on_receive_request(
                async move |req: ModelCall, responder, _cx: ConnectionTo<AcpAgentRole>| {
                    if model_calls.fetch_add(1, Ordering::AcqRel) == 0 {
                        assert!(
                            req.0
                                .messages
                                .iter()
                                .any(|message| message.role == MessageRole::System
                                    && message.content.contains("[Notification] idle context"))
                        );
                        *model_pending.lock().await = Some(responder);
                        model_entered.notify_one();
                        Ok(())
                    } else {
                        // Canonical Agent normalization merges system context
                        // into one leading system message before the model call.
                        let notifications: Vec<_> = req
                            .0
                            .messages
                            .iter()
                            .filter(|message| message.role == MessageRole::System)
                            .flat_map(|message| message.content.lines())
                            .filter(|line| line.starts_with("[Notification]"))
                            .collect();
                        assert_eq!(notifications.len(), FOLD_QUEUE_MAX_EVENTS + 1);
                        assert_eq!(notifications[1], "[Notification] folded 2");
                        assert_eq!(*notifications.last().unwrap(), "[Notification] folded 17");
                        assert!(
                            !req.0
                                .messages
                                .iter()
                                .any(|message| message.content.contains("dropped event"))
                        );
                        responder.respond(ModelResponse(response("after folded events")))
                    }
                },
                on_receive_request!(),
            )
            .connect_with(Transport, |cx: ConnectionTo<AcpAgentRole>| async move {
                cx.send_request(initialize()).block_task().await?;
                let session = cx
                    .send_request(NewSessionRequest::new(PathBuf::from("/")))
                    .block_task()
                    .await?
                    .session_id;
                let idle = cx
                    .send_request(NotifyRequest {
                        session_id: session.clone(),
                        events: vec!["idle context".into()],
                        auto_respond: false,
                        if_busy: NotifyIfBusy::Drop,
                    })
                    .block_task()
                    .await?;
                assert!(idle.queued && !idle.busy);
                assert_eq!(calls.load(Ordering::Acquire), 0);
                let notifier = cx.clone();
                let notify_session = session.clone();
                let task = tokio::spawn(async move {
                    entered.notified().await;
                    let dropped = notifier
                        .send_request(NotifyRequest {
                            session_id: notify_session.clone(),
                            events: vec!["dropped event".into()],
                            auto_respond: true,
                            if_busy: NotifyIfBusy::Drop,
                        })
                        .block_task()
                        .await
                        .unwrap();
                    assert!(!dropped.queued && dropped.busy);
                    let folded = notifier
                        .send_request(NotifyRequest {
                            session_id: notify_session,
                            events: (0..18).map(|i| format!("folded {i}")).collect(),
                            auto_respond: true,
                            if_busy: NotifyIfBusy::Fold,
                        })
                        .block_task()
                        .await
                        .unwrap();
                    assert!(folded.queued && folded.busy);
                    pending
                        .lock()
                        .await
                        .take()
                        .unwrap()
                        .respond(ModelResponse(response("first reply")))
                        .unwrap();
                });
                assert_eq!(
                    cx.send_request(prompt(session.clone(), "first prompt"))
                        .block_task()
                        .await?
                        .stop_reason,
                    StopReason::EndTurn
                );
                task.await.unwrap();
                assert_eq!(
                    cx.send_request(prompt(session, "second prompt"))
                        .block_task()
                        .await?
                        .stop_reason,
                    StopReason::EndTurn
                );
                assert_eq!(calls.load(Ordering::Acquire), 2);
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn host_managed_auto_response_does_not_retain_a_synthetic_user_request() {
        let calls = Arc::new(AtomicUsize::new(0));
        let model_calls = calls.clone();
        Client
            .builder()
            .on_receive_notification(
                async |_req: SessionNotification, _cx: ConnectionTo<AcpAgentRole>| Ok(()),
                on_receive_notification!(),
            )
            .on_receive_request(
                async |_req: ListTools, responder, _cx: ConnectionTo<AcpAgentRole>| {
                    responder.respond(ToolsResponse(ToolsListResponse { tools: Vec::new() }))
                },
                on_receive_request!(),
            )
            .on_receive_request(
                async move |req: ModelCall, responder, _cx: ConnectionTo<AcpAgentRole>| {
                    let index = model_calls.fetch_add(1, Ordering::AcqRel);
                    let users: Vec<_> = req
                        .0
                        .messages
                        .iter()
                        .filter(|message| message.role == MessageRole::User)
                        .collect();
                    assert_eq!(users.len(), 1);
                    if index == 0 {
                        assert_eq!(users[0].content, AUTO_RESPOND_INSTRUCTION);
                    } else {
                        assert_eq!(users[0].content, "real follow-up");
                        assert!(
                            req.0
                                .messages
                                .iter()
                                .any(|message| message.role == MessageRole::Assistant
                                    && message.content == "automatic reply")
                        );
                        assert!(
                            !req.0
                                .messages
                                .iter()
                                .any(|message| message.content == AUTO_RESPOND_INSTRUCTION)
                        );
                    }
                    assert!(
                        req.0
                            .messages
                            .iter()
                            .any(|message| message.role == MessageRole::System
                                && message.content.contains("[Notification] task completed"))
                    );
                    responder.respond(ModelResponse(response(if index == 0 {
                        "automatic reply"
                    } else {
                        "follow-up reply"
                    })))
                },
                on_receive_request!(),
            )
            .connect_with(Transport, |cx: ConnectionTo<AcpAgentRole>| async move {
                cx.send_request(initialize()).block_task().await?;
                let session = cx
                    .send_request(NewSessionRequest::new(PathBuf::from("/")))
                    .block_task()
                    .await?
                    .session_id;
                let acknowledged = cx
                    .send_request(NotifyRequest {
                        session_id: session.clone(),
                        events: vec!["task completed".into()],
                        auto_respond: true,
                        if_busy: NotifyIfBusy::Drop,
                    })
                    .block_task()
                    .await?;
                assert!(acknowledged.queued && !acknowledged.busy);
                // The notify acknowledgement intentionally precedes its turn.
                let follow_up = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    loop {
                        match cx
                            .send_request(prompt(session.clone(), "real follow-up"))
                            .block_task()
                            .await
                        {
                            Ok(response) => break response,
                            Err(error) => {
                                assert!(error.to_string().contains("already running"));
                                tokio::task::yield_now().await;
                            }
                        }
                    }
                })
                .await
                .unwrap();
                assert_eq!(follow_up.stop_reason, StopReason::EndTurn);
                assert_eq!(calls.load(Ordering::Acquire), 2);
                Ok(())
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn host_managed_truncation_is_an_error_and_retains_actual_partial_context() {
        let calls = Arc::new(AtomicUsize::new(0));
        let model_calls = calls.clone();
        Client
            .builder()
            .on_receive_notification(
                async |_req: SessionNotification, _cx: ConnectionTo<AcpAgentRole>| Ok(()),
                on_receive_notification!(),
            )
            .on_receive_request(
                async |_req: ListTools, responder, _cx: ConnectionTo<AcpAgentRole>| {
                    responder.respond(ToolsResponse(ToolsListResponse { tools: Vec::new() }))
                },
                on_receive_request!(),
            )
            .on_receive_request(
                async move |req: ModelCall, responder, _cx: ConnectionTo<AcpAgentRole>| {
                    let mut result = response("partial answer");
                    if model_calls.fetch_add(1, Ordering::AcqRel) == 0 {
                        result.stop_reason = octos_llm::StopReason::MaxTokens;
                    } else {
                        assert!(
                            req.0
                                .messages
                                .iter()
                                .any(|message| message.role == MessageRole::Assistant
                                    && message.content == "partial answer")
                        );
                        result.content = Some("completed later".into());
                    }
                    responder.respond(ModelResponse(result))
                },
                on_receive_request!(),
            )
            .connect_with(Transport, |cx: ConnectionTo<AcpAgentRole>| async move {
                cx.send_request(initialize()).block_task().await?;
                let session = cx
                    .send_request(NewSessionRequest::new(PathBuf::from("/")))
                    .block_task()
                    .await?
                    .session_id;
                let error = cx
                    .send_request(prompt(session.clone(), "first prompt"))
                    .block_task()
                    .await
                    .unwrap_err();
                assert!(error.to_string().contains("incomplete"));
                assert_eq!(
                    cx.send_request(prompt(session, "continue"))
                        .block_task()
                        .await?
                        .stop_reason,
                    StopReason::EndTurn
                );
                assert_eq!(calls.load(Ordering::Acquire), 2);
                Ok(())
            })
            .await
            .unwrap();
    }
}
