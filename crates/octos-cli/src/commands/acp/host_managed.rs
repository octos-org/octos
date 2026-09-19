//! Confined ACP execution with one host-owned, memory-only session.

use super::*;
use octos_agent::{Tool, ToolResult};
use octos_llm::host::{self, HostConfig, ModelRequest, ToolCallRequest, ToolCallResponse, ToolsListResponse};
use octos_llm::{ChatConfig, ChatResponse, ToolSpec};
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::sync::OnceLock;
use tokio::sync::Notify;

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
    async fn request<R: agent_client_protocol::JsonRpcRequest>(&self, request: R) -> Result<R::Response> {
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
    async fn chat(&self, messages: &[octos_core::Message], tools: &[ToolSpec], config: &ChatConfig) -> Result<ChatResponse> {
        let request = ModelRequest { messages: messages.to_vec(), tools: tools.to_vec(), config: config.clone() };
        request.validate().map_err(eyre::Report::msg)?;
        let response = self.broker.request(ModelCall(request)).await?.0;
        host::validate_payload_size(&response).map_err(eyre::Report::msg)?;
        Ok(response)
    }

    async fn chat_stream(&self, messages: &[octos_core::Message], tools: &[ToolSpec], config: &ChatConfig) -> Result<octos_llm::ChatStream> {
        Ok(host::response_stream(self.chat(messages, tools, config).await?))
    }

    fn model_id(&self) -> &str { &self.model.model_id }
    fn provider_name(&self) -> &str { &self.model.provider_name }
    fn context_window(&self) -> u32 { self.model.context_window }
    fn max_output_tokens(&self) -> u32 { self.model.max_output_tokens }
}

struct HostTool {
    spec: ToolSpec,
    broker: Broker,
}

#[async_trait::async_trait]
impl Tool for HostTool {
    fn name(&self) -> &str { &self.spec.name }
    fn description(&self) -> &str { &self.spec.description }
    fn input_schema(&self) -> serde_json::Value { self.spec.input_schema.clone() }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let request = ToolCallRequest { name: self.spec.name.clone(), arguments: args.clone() };
        host::validate_payload_size(&request).map_err(eyre::Report::msg)?;
        let response = self.broker.request(CallTool(request)).await?.0;
        host::validate_payload_size(&response).map_err(eyre::Report::msg)?;
        Ok(ToolResult { output: response.content, success: !response.is_error, ..Default::default() })
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
            config: OnceLock::new(), session_created: AtomicBool::new(false),
            turn_started: AtomicBool::new(false), lifetime: Arc::new(Lifetime::default()),
            agent_generation: AtomicU64::new(0),
            sessions: Arc::new(Mutex::new(HashMap::new())), max_iterations,
            session_update: Mutex::new(()),
        }
    }

    fn initialize(&self, request: &InitializeRequest, sandbox: &str) -> Result<InitializeResponse> {
        let value = request.client_capabilities.meta.as_ref()
            .and_then(|meta| meta.get(host::CAPABILITY_KEY))
            .ok_or_else(|| eyre::eyre!("host-managed broker capability is required"))?;
        let config: HostConfig = serde_json::from_value(value.clone())?;
        config.validate().map_err(eyre::Report::msg)?;
        self.config.set(config).map_err(|_| eyre::eyre!("already initialized"))?;
        let mut response = build_initialize_response(request);
        response.agent_capabilities.load_session = false;
        response.agent_capabilities.meta = Some([(host::CAPABILITY_KEY.to_string(), serde_json::to_value(host::HostCapabilities {
            version: host::VERSION, confined: true, sandbox: sandbox.into(),
        })?)].into_iter().collect());
        Ok(response)
    }

    fn agent(&self, broker: Broker, specs: Vec<ToolSpec>, memory: Arc<EpisodeStore>) -> Result<Arc<Agent>> {
        let config = self.config.get().ok_or_else(|| eyre::eyre!("initialize is required"))?;
        let provider = Arc::new(HostProvider { broker: broker.clone(), model: config.model.clone() });
        let mut tools = ToolRegistry::new();
        for spec in specs {
            tools.register(HostTool { spec, broker: broker.clone() });
        }
        let agent_config = AgentConfig {
            max_iterations: self.max_iterations,
            chat_max_tokens: Some(config.model.max_output_tokens),
            save_episodes: false, suppress_auto_send_files: true, format_after_edit: false,
            ..Default::default()
        };
        Ok(Arc::new(Agent::new(AgentId::new("host-managed"), provider, tools, memory)
            .with_config(agent_config).with_shutdown(self.lifetime.shutdown.clone())
            .with_system_prompt(config.system_prompt.clone())))
    }

    async fn new_session(&self, request: NewSessionRequest, connection: ConnectionTo<Client>) -> Result<NewSessionResponse> {
        eyre::ensure!(self.config.get().is_some(), "initialize is required");
        eyre::ensure!(request.mcp_servers.is_empty(), "host-managed sessions reject external MCP servers");
        eyre::ensure!(self.session_created.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok(),
            "host-managed processes allow exactly one session");
        let broker = Broker { connection, lifetime: self.lifetime.clone(), generation: 0 };
        let agent = self.agent(broker, Vec::new(), Arc::new(EpisodeStore::in_memory()?))?;
        let session_id = new_session_id();
        let session = Arc::new(AcpSession {
            agent, session_key: SessionKey::with_profile("host-managed", "acp", session_id.0.as_ref()),
            session_store: None, history: Mutex::new(Vec::new()), shutdown: self.lifetime.shutdown.clone(),
            active_turns: AtomicUsize::new(0), fold_queue: Mutex::new(Vec::new()),
        });
        self.sessions.lock().await.insert(session_id.clone(), session);
        Ok(NewSessionResponse::new(session_id))
    }

    async fn session(&self, id: &SessionId) -> Result<Arc<AcpSession>> {
        self.sessions.lock().await.get(id).cloned().ok_or_else(|| eyre::eyre!("unknown host-managed session"))
    }

    /// Refresh the host's tool snapshot between turns. The immutable Agent
    /// registry is replaced only while idle; history and the in-memory store
    /// remain in this one session. Every call still rechecks the host registry.
    async fn refresh(&self, id: &SessionId, connection: ConnectionTo<Client>, generation: u64) -> Result<Arc<AcpSession>> {
        let broker = Broker { connection, lifetime: self.lifetime.clone(), generation };
        let mut specs = broker.tools().await?;
        self.lifetime.check(generation)?;
        let _update = self.session_update.lock().await;
        let old = self.session(id).await?;
        let mut current = old.agent.tool_registry().specs();
        specs.sort_by(|left, right| left.name.cmp(&right.name));
        current.sort_by(|left, right| left.name.cmp(&right.name));
        if self.agent_generation.load(Ordering::Acquire) == generation
            && current.len() == specs.len() && current.iter().zip(&specs).all(|(left, right)| {
            left.name == right.name && left.description == right.description && left.input_schema == right.input_schema
        }) {
            return Ok(old);
        }
        let agent = self.agent(broker, specs, old.agent.memory_store().clone())?;
        let session = Arc::new(AcpSession {
            agent, session_key: old.session_key.clone(), session_store: None,
            history: Mutex::new(std::mem::take(&mut *old.history.lock().await)),
            shutdown: self.lifetime.shutdown.clone(), active_turns: AtomicUsize::new(1),
            fold_queue: Mutex::new(std::mem::take(&mut *old.fold_queue.lock().await)),
        });
        self.sessions.lock().await.insert(id.clone(), session.clone());
        self.agent_generation.store(generation, Ordering::Release);
        Ok(session)
    }

    fn begin_turn(self: &Arc<Self>) -> Result<TurnGuard> {
        eyre::ensure!(self.turn_started.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok(),
            "a host-managed turn is already running");
        self.lifetime.shutdown.store(false, Ordering::Release);
        Ok(TurnGuard(self.clone()))
    }
}

struct TurnGuard(Arc<ManagedState>);

impl Drop for TurnGuard {
    fn drop(&mut self) { self.0.turn_started.store(false, Ordering::Release); }
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
    let result = AcpAgentRole.builder().name("octos-host-managed")
        .on_receive_request(async move |req: InitializeRequest, responder, _cx: ConnectionTo<Client>| {
            match init.initialize(&req, sandbox) {
                Ok(response) => responder.respond(response),
                Err(e) => responder.respond_with_error(error(e)),
            }
        }, on_receive_request!())
        .on_receive_request(async move |req: NewSessionRequest, responder, cx: ConnectionTo<Client>| {
            match new.new_session(req, cx).await {
                Ok(response) => responder.respond(response),
                Err(e) => responder.respond_with_error(error(e)),
            }
        }, on_receive_request!())
        .on_receive_request(async move |_req: LoadSessionRequest, responder, _cx: ConnectionTo<Client>| {
            responder.respond_with_error(error("host-managed sessions cannot load persistent history"))
        }, on_receive_request!())
        .on_receive_request(async move |req: PromptRequest, responder: agent_client_protocol::Responder<PromptResponse>, cx: ConnectionTo<Client>| {
            if let Err(e) = host::validate_payload_size(&req) {
                return responder.respond_with_error(error(e));
            }
            let session = match prompt.session(&req.session_id).await {
                Ok(session) => session,
                Err(e) => return responder.respond_with_error(error(e)),
            };
            let guard = match prompt.begin_turn() {
                Ok(guard) => guard,
                Err(e) => return responder.respond_with_error(error(e)),
            };
            session.active_turns.store(1, Ordering::Release);
            let generation = prompt.lifetime.generation.load(Ordering::Acquire);
            let state = prompt.clone();
            cx.clone().spawn(async move {
                let _guard = guard;
                match state.refresh(&req.session_id, cx.clone(), generation).await {
                    Ok(session) => run_prompt_turn(session, req, cx, responder).await,
                    Err(e) => {
                        session.active_turns.store(0, Ordering::Release);
                        if state.lifetime.check(generation).is_err() {
                            responder.respond(PromptResponse::new(StopReason::Cancelled))
                        } else {
                            responder.respond_with_error(error(e))
                        }
                    }
                }
            })
        }, on_receive_request!())
        .on_receive_request(async move |mut req: NotifyRequest, responder: agent_client_protocol::Responder<NotifyResponse>, cx: ConnectionTo<Client>| {
            if let Err(e) = host::validate_payload_size(&req) {
                return responder.respond_with_error(error(e));
            }
            if req.events.is_empty() {
                return responder.respond_with_error(error("host notification events must not be empty"));
            }
            let _update = notify.session_update.lock().await;
            let session = match notify.session(&req.session_id).await {
                Ok(session) => session,
                Err(e) => return responder.respond_with_error(error(e)),
            };
            if !req.auto_respond || notify.turn_started.load(Ordering::Acquire) {
                // The turn guard covers tool refresh as well as inference.
                // Never let the ordinary handler start an unguarded turn.
                req.auto_respond = false;
                return handle_notify(&notify.sessions, req, cx, responder).await;
            }
            let guard = match notify.begin_turn() {
                Ok(guard) => guard,
                Err(e) => return responder.respond_with_error(error(e)),
            };
            append_host_events(&session, &req.events).await;
            session.active_turns.store(1, Ordering::Release);
            let generation = notify.lifetime.generation.load(Ordering::Acquire);
            let state = notify.clone();
            responder.respond(NotifyResponse { queued: true, busy: false })?;
            cx.clone().spawn(async move {
                let _guard = guard;
                if let Ok(session) = state.refresh(&req.session_id, cx.clone(), generation).await {
                    let _ = run_turn_core(&session, &req.session_id, AUTO_RESPOND_INSTRUCTION, true, &cx).await;
                    session.active_turns.store(0, Ordering::Release);
                } else {
                    session.active_turns.store(0, Ordering::Release);
                }
                Ok(())
            })
        }, on_receive_request!())
        .on_receive_notification(async move |req: CancelNotification, _cx: ConnectionTo<Client>| {
            if cancel.session(&req.session_id).await.is_ok() {
                cancel.lifetime.cancel();
            }
            Ok(())
        }, on_receive_notification!())
        .connect_to(transport).await;
    state.lifetime.cancel();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{ClientCapabilities, TextContent};
    use agent_client_protocol::schema::ProtocolVersion;
    use octos_core::MessageRole;

    struct Transport;

    impl agent_client_protocol::ConnectTo<Client> for Transport {
        async fn connect_to(self, client: impl agent_client_protocol::ConnectTo<AcpAgentRole> + 'static) -> std::result::Result<(), AcpError> {
            serve(4, "test-only", client).await
        }
    }

    fn initialize() -> InitializeRequest {
        let config = HostConfig {
            version: host::VERSION,
            model: host::HostModel {
                model_id: "host-test".into(), provider_name: "broker".into(),
                context_window: 32_000, max_output_tokens: 1024,
            },
            system_prompt: "Use only tools supplied by the host.".into(),
        };
        let mut capabilities = ClientCapabilities::new();
        capabilities.meta = Some([(host::CAPABILITY_KEY.into(), serde_json::to_value(config).unwrap())].into_iter().collect());
        InitializeRequest::new(ProtocolVersion::V1).client_capabilities(capabilities)
    }

    fn prompt(session: SessionId, text: &str) -> PromptRequest {
        PromptRequest::new(session, vec![ContentBlock::Text(TextContent::new(text))])
    }

    fn response(content: &str) -> ChatResponse {
        ChatResponse {
            content: Some(content.into()), reasoning_content: None, tool_calls: Vec::new(),
            stop_reason: octos_llm::StopReason::EndTurn, usage: Default::default(), provider_index: None,
        }
    }

    fn tool(name: &str) -> ToolSpec {
        ToolSpec { name: name.into(), description: "Host operation".into(), input_schema: serde_json::json!({"type":"object"}) }
    }

    #[test]
    fn host_managed_old_generation_stays_cancelled_after_next_turn_starts() {
        let lifetime = Lifetime::default();
        assert!(lifetime.check(0).is_ok());
        lifetime.cancel();
        lifetime.shutdown.store(false, Ordering::Release);
        assert!(lifetime.check(0).is_err());
        assert!(lifetime.check(1).is_ok());
        let command = AcpCommand { host_managed: true, config: Some("/never-read-this-config".into()), ..Default::default() };
        assert!(command.factory().err().unwrap().to_string().contains("confined ACP broker transport"));
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
        Client.builder().connect_with(Transport, |cx: ConnectionTo<AcpAgentRole>| async move {
            cx.send_request(initialize()).block_task().await?;
            let session = cx.send_request(NewSessionRequest::new(PathBuf::from("/"))).block_task().await?.session_id;
            assert!(cx.send_request(prompt(session, "private data")).block_task().await.is_err());
            Ok(())
        }).await.unwrap();
    }

    #[tokio::test]
    async fn host_managed_routes_model_and_tools_and_refreshes_tool_snapshot() {
        let model_calls = Arc::new(AtomicUsize::new(0));
        let tool_calls = Arc::new(AtomicUsize::new(0));
        let lists = Arc::new(AtomicUsize::new(0));
        let model_count = model_calls.clone();
        let tool_count = tool_calls.clone();
        let list_count = lists.clone();
        Client.builder()
            .on_receive_notification(async |_req: SessionNotification, _cx: ConnectionTo<AcpAgentRole>| Ok(()), on_receive_notification!())
            .on_receive_request(async move |_req: ListTools, responder, _cx: ConnectionTo<AcpAgentRole>| {
                let name = if list_count.fetch_add(1, Ordering::AcqRel) == 0 { "echo" } else { "changed" };
                responder.respond(ToolsResponse(ToolsListResponse { tools: vec![tool(name)] }))
            }, on_receive_request!())
            .on_receive_request(async move |req: CallTool, responder, _cx: ConnectionTo<AcpAgentRole>| {
                assert_eq!(req.0.name, "echo");
                assert_eq!(req.0.arguments["text"], "private tool input");
                tool_count.fetch_add(1, Ordering::AcqRel);
                responder.respond(ToolResponse(ToolCallResponse { content: "private tool feedback".into(), is_error: false }))
            }, on_receive_request!())
            .on_receive_request(async move |req: ModelCall, responder, _cx: ConnectionTo<AcpAgentRole>| {
                let index = model_count.fetch_add(1, Ordering::AcqRel);
                assert_eq!(req.0.tools.len(), 1, "native Octos tools must not be registered");
                assert_eq!(req.0.config.max_tokens, Some(1024));
                let mut result = response("done");
                if index == 0 {
                    assert_eq!(req.0.tools[0].name, "echo");
                    result.tool_calls.push(octos_core::ToolCall {
                        id: "call-1".into(), name: "echo".into(),
                        arguments: serde_json::json!({"text":"private tool input"}), metadata: None,
                    });
                    result.stop_reason = octos_llm::StopReason::ToolUse;
                } else if index == 1 {
                    assert!(req.0.messages.iter().any(|message| message.role == MessageRole::Tool && message.content.contains("private tool feedback")));
                } else {
                    assert_eq!(req.0.tools[0].name, "changed");
                    assert!(req.0.messages.iter().any(|message| message.content.contains("private tool feedback")), "tool refresh must preserve history");
                }
                responder.respond(ModelResponse(result))
            }, on_receive_request!())
            .connect_with(Transport, |cx: ConnectionTo<AcpAgentRole>| async move {
                cx.send_request(initialize()).block_task().await?;
                let session = cx.send_request(NewSessionRequest::new(PathBuf::from("/"))).block_task().await?.session_id;
                assert_eq!(cx.send_request(prompt(session.clone(), "first")).block_task().await?.stop_reason, StopReason::EndTurn);
                assert_eq!(cx.send_request(prompt(session, "second")).block_task().await?.stop_reason, StopReason::EndTurn);
                Ok(())
            }).await.unwrap();
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
        Client.builder()
            .on_receive_notification(async |_req: SessionNotification, _cx: ConnectionTo<AcpAgentRole>| Ok(()), on_receive_notification!())
            .on_receive_request(async |_req: ListTools, responder, _cx: ConnectionTo<AcpAgentRole>| {
                responder.respond(ToolsResponse(ToolsListResponse { tools: Vec::new() }))
            }, on_receive_request!())
            .on_receive_request(async move |_req: ModelCall, responder, _cx: ConnectionTo<AcpAgentRole>| {
                if model_calls.fetch_add(1, Ordering::AcqRel) == 0 {
                    *pending_model.lock().await = Some(responder);
                    entered_model.notify_one();
                    Ok(())
                } else {
                    responder.respond(ModelResponse(response("fresh")))
                }
            }, on_receive_request!())
            .connect_with(Transport, |cx: ConnectionTo<AcpAgentRole>| async move {
                cx.send_request(initialize()).block_task().await?;
                let session = cx.send_request(NewSessionRequest::new(PathBuf::from("/"))).block_task().await?.session_id;
                let canceller = cx.clone();
                let cancel_session = session.clone();
                let task = tokio::spawn(async move {
                    entered.notified().await;
                    assert!(canceller.send_request(prompt(cancel_session.clone(), "overlapping turn")).block_task().await.is_err());
                    canceller.send_notification(CancelNotification::new(cancel_session)).unwrap();
                });
                let first = tokio::time::timeout(std::time::Duration::from_secs(5), cx.send_request(prompt(session.clone(), "cancel this")).block_task()).await.unwrap()?;
                assert_eq!(first.stop_reason, StopReason::Cancelled);
                task.await.unwrap();
                assert_eq!(cx.send_request(prompt(session, "fresh turn")).block_task().await?.stop_reason, StopReason::EndTurn);
                Ok(())
            }).await.unwrap();
        assert_eq!(calls.load(Ordering::Acquire), 2, "cancelled calls must not fall back to a second broker request");
    }
}
