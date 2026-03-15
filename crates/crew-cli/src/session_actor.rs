//! Session actor: per-session tokio task that owns tools and processes messages.
//!
//! Replaces the spawn-per-message model in the gateway, eliminating the
//! `set_context()` race condition where shared tools could route messages
//! to the wrong chat.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crew_agent::tools::{MessageTool, SendFileTool, SpawnTool, ToolPolicy, ToolRegistry};
use crew_agent::{Agent, AgentConfig, HookContext, HookExecutor, TokenTracker};
use crew_bus::{ActiveSessionStore, SessionHandle, SessionManager};
use crew_core::AgentId;
use crew_core::{InboundMessage, Message, MessageRole, OutboundMessage, SessionKey};
use crew_llm::{
    AdaptiveMode, AdaptiveRouter, EmbeddingProvider, LlmProvider, ProviderRouter,
    ResponsivenessObserver,
};
use crew_memory::{EpisodeStore, MemoryStore};
use tokio::sync::{Mutex, Semaphore, mpsc};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::config::QueueMode;
use crate::cron_tool::CronTool;
use crate::status_layers::{StatusComposer, UserStatusConfig};

/// Default actor inbox capacity.
const ACTOR_INBOX_SIZE: usize = 32;

/// Default idle timeout before an actor shuts down (30 minutes).
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 1800;

/// Maximum concurrent overflow tasks per session.
const MAX_OVERFLOW_TASKS: u32 = 5;

/// Maximum number of pending messages buffered per inactive session.
const MAX_PENDING_PER_SESSION: usize = 50;

/// Shared buffer of outbound messages from inactive sessions, keyed by session key string.
/// Flushed when the user switches to that session via `/s`.
pub type PendingMessages = Arc<Mutex<HashMap<String, Vec<OutboundMessage>>>>;

// ── Messages ────────────────────────────────────────────────────────────────

/// Messages dispatched to a session actor.
pub enum ActorMessage {
    /// A user message to process.
    Inbound {
        message: InboundMessage,
        image_media: Vec<String>,
    },
    /// Result from a background subagent task — injected as a system message
    /// into the conversation without triggering an extra LLM call.
    BackgroundResult {
        /// Task identifier for attribution.
        task_label: String,
        /// The subagent's final output.
        content: String,
    },
    /// Cancel the current operation.
    Cancel,
}

// ── ActorHandle ─────────────────────────────────────────────────────────────

/// Handle to a running session actor.
pub struct ActorHandle {
    pub tx: mpsc::Sender<ActorMessage>,
    pub created_at: Instant,
    join_handle: JoinHandle<()>,
}

impl ActorHandle {
    /// Whether the actor task has completed (idle-timeout, panic, etc.).
    pub fn is_finished(&self) -> bool {
        self.join_handle.is_finished()
    }
}

// ── ActorRegistry ───────────────────────────────────────────────────────────

/// Manages the lifecycle of session actors.
pub struct ActorRegistry {
    actors: HashMap<String, ActorHandle>,
    factory: ActorFactory,
    semaphore: Arc<Semaphore>,
    out_tx: mpsc::Sender<OutboundMessage>,
    pending_messages: PendingMessages,
}

impl ActorRegistry {
    pub fn new(
        factory: ActorFactory,
        semaphore: Arc<Semaphore>,
        out_tx: mpsc::Sender<OutboundMessage>,
        pending_messages: PendingMessages,
    ) -> Self {
        Self {
            actors: HashMap::new(),
            factory,
            semaphore,
            out_tx,
            pending_messages,
        }
    }

    /// Route an inbound message to the correct actor, creating one if needed.
    pub async fn dispatch(
        &mut self,
        message: InboundMessage,
        image_media: Vec<String>,
        session_key: SessionKey,
        reply_channel: &str,
        reply_chat_id: &str,
        status_indicator: Option<Arc<StatusComposer>>,
    ) {
        let key_str = session_key.to_string();

        // If actor exists but has finished (idle-timeout/panic), remove it
        if let Some(handle) = self.actors.get(&key_str) {
            if handle.is_finished() {
                self.actors.remove(&key_str);
            }
        }

        // Create actor if needed
        if !self.actors.contains_key(&key_str) {
            let (tx, join_handle) = self.factory.spawn(
                session_key.clone(),
                reply_channel,
                reply_chat_id,
                self.semaphore.clone(),
                status_indicator.clone(),
            );
            self.actors.insert(
                key_str.clone(),
                ActorHandle {
                    tx,
                    created_at: Instant::now(),
                    join_handle,
                },
            );
        }

        let handle = self.actors.get(&key_str).unwrap();
        let actor_msg = ActorMessage::Inbound {
            message,
            image_media,
        };

        match handle.tx.try_send(actor_msg) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(actor_msg)) => {
                // Actor inbox is full — send backpressure feedback
                let _ = self
                    .out_tx
                    .send(OutboundMessage {
                        channel: reply_channel.to_string(),
                        chat_id: reply_chat_id.to_string(),
                        content: "⏳ Still processing, your message is queued...".to_string(),
                        reply_to: None,
                        media: vec![],
                        metadata: serde_json::json!({}),
                    })
                    .await;
                // Now block until space is available
                let handle = self.actors.get(&key_str).unwrap();
                let _ = handle.tx.send(actor_msg).await;
            }
            Err(mpsc::error::TrySendError::Closed(actor_msg)) => {
                // Actor died — remove and create a new one
                self.actors.remove(&key_str);
                let (tx, join_handle) = self.factory.spawn(
                    session_key,
                    reply_channel,
                    reply_chat_id,
                    self.semaphore.clone(),
                    status_indicator,
                );
                let _ = tx.send(actor_msg).await;
                self.actors.insert(
                    key_str,
                    ActorHandle {
                        tx,
                        created_at: Instant::now(),
                        join_handle,
                    },
                );
            }
        }
    }

    /// Remove actors whose tasks have completed.
    pub fn reap_dead_actors(&mut self) {
        self.actors.retain(|key, handle| {
            if handle.is_finished() {
                debug!(session = %key, "reaping completed actor");
                false
            } else {
                true
            }
        });
    }

    /// Cancel a specific session actor.
    pub async fn cancel(&self, session_key: &str) {
        if let Some(handle) = self.actors.get(session_key) {
            let _ = handle.tx.send(ActorMessage::Cancel).await;
        }
    }

    /// Shut down all actors gracefully.
    pub async fn shutdown_all(self) {
        // Drop all senders — actors will exit on recv() returning None
        let handles: Vec<_> = self
            .actors
            .into_values()
            .map(|h| {
                drop(h.tx);
                h.join_handle
            })
            .collect();

        for h in handles {
            let _ = h.await;
        }
    }

    /// Flush buffered messages for a session key (called on `/s` switch).
    /// Returns the number of messages flushed.
    pub async fn flush_pending(&self, session_key: &str) -> usize {
        let messages = self
            .pending_messages
            .lock()
            .await
            .remove(session_key)
            .unwrap_or_default();
        let count = messages.len();
        for msg in messages {
            let _ = self.out_tx.send(msg).await;
        }
        count
    }

    /// Number of active actors.
    pub fn len(&self) -> usize {
        self.actors.len()
    }

    /// Whether there are no active actors.
    pub fn is_empty(&self) -> bool {
        self.actors.is_empty()
    }
}

// ── ActorFactory ────────────────────────────────────────────────────────────

/// Shared resources needed to create per-session actors.
pub struct ActorFactory {
    pub agent_config: AgentConfig,
    pub llm: Arc<dyn LlmProvider>,
    pub llm_for_compaction: Arc<dyn LlmProvider>,
    pub memory: Arc<EpisodeStore>,
    pub system_prompt: Arc<std::sync::RwLock<String>>,
    pub hooks: Option<Arc<HookExecutor>>,
    pub hook_context_template: Option<HookContext>,
    /// Data directory for creating per-actor SessionHandle instances.
    pub data_dir: std::path::PathBuf,
    /// Shared SessionManager for admin operations (/sessions, /new, /delete).
    /// NOT used by actors — only by the gateway main loop.
    pub session_mgr: Arc<Mutex<SessionManager>>,
    pub out_tx: mpsc::Sender<OutboundMessage>,
    pub spawn_inbound_tx: mpsc::Sender<InboundMessage>,
    pub cron_service: Option<Arc<crew_bus::CronService>>,
    pub tool_registry_factory: Arc<dyn ToolRegistryFactory + Send + Sync>,
    pub pipeline_factory: Option<Arc<dyn PipelineToolFactory + Send + Sync>>,
    pub max_history: Arc<std::sync::atomic::AtomicUsize>,
    pub idle_timeout: Duration,
    pub session_timeout: Duration,
    pub shutdown: Arc<AtomicBool>,
    /// Working directory for SpawnTool (shared profile-level cwd).
    pub cwd: std::path::PathBuf,
    /// Sandbox config — used to create per-user sandbox instances.
    pub sandbox_config: crew_agent::SandboxConfig,
    /// Provider policy for SpawnTool and PipelineTool.
    pub provider_policy: Option<ToolPolicy>,
    /// Worker system prompt for SpawnTool subagents.
    pub worker_prompt: Option<String>,
    /// Provider router for SpawnTool and PipelineTool.
    pub provider_router: Option<Arc<ProviderRouter>>,
    /// Optional embedder for episodic memory recall.
    pub embedder: Option<Arc<dyn EmbeddingProvider>>,
    /// Active session store — used to check if a session is currently active.
    pub active_sessions: Arc<Mutex<ActiveSessionStore>>,
    /// Pending message buffer — replies from inactive sessions are held here.
    pub pending_messages: PendingMessages,
    /// Queue mode for handling messages arriving during active agent runs.
    pub queue_mode: QueueMode,
    /// Side-channel to the AdaptiveRouter for responsiveness feedback.
    /// None when adaptive routing is disabled or using a static provider chain.
    pub adaptive_router: Option<Arc<AdaptiveRouter>>,
    /// Memory store for saving long-form outputs (research reports) to the
    /// memory bank so only a summary is injected into session context.
    pub memory_store: Option<Arc<MemoryStore>>,
}

/// Trait for creating per-session ToolRegistry instances.
///
/// This abstracts the complex tool registration logic (builtins, plugins, MCP,
/// policies, etc.) so the actor module doesn't depend on all those details.
pub trait ToolRegistryFactory: Send + Sync {
    /// Create a base ToolRegistry with all non-session-specific tools registered.
    /// The caller will add session-specific tools (MessageTool, SendFileTool, etc.)
    fn create_base_registry(&self) -> ToolRegistry;

    /// Create a base ToolRegistry with cwd-bound tools re-bound to a per-user
    /// workspace directory. Non-cwd tools (web, MCP, plugins) are preserved.
    /// The sandbox is created fresh for the per-user workspace path.
    fn create_registry_for_workspace(
        &self,
        workspace: &std::path::Path,
        sandbox: Box<dyn crew_agent::Sandbox>,
    ) -> ToolRegistry;
}

/// Trait for creating per-session pipeline tool instances.
pub trait PipelineToolFactory: Send + Sync {
    fn create(&self) -> Arc<dyn crew_agent::tools::Tool>;
}

/// ToolRegistryFactory backed by snapshot_excluding() — clones shared tools cheaply.
pub struct SnapshotToolRegistryFactory {
    base: ToolRegistry,
}

impl SnapshotToolRegistryFactory {
    pub fn new(base: ToolRegistry) -> Self {
        Self { base }
    }
}

impl ToolRegistryFactory for SnapshotToolRegistryFactory {
    fn create_base_registry(&self) -> ToolRegistry {
        // Clone all tools (Arc refcount bumps, cheap)
        self.base.snapshot_excluding(&[])
    }

    fn create_registry_for_workspace(
        &self,
        workspace: &std::path::Path,
        sandbox: Box<dyn crew_agent::Sandbox>,
    ) -> ToolRegistry {
        // Re-bind cwd-bound tools to the per-user workspace while
        // preserving non-cwd tools (web_search, browser, MCP, plugins, etc.)
        self.base.rebind_cwd(workspace, sandbox)
    }
}

impl ActorFactory {
    /// Spawn a new session actor, returning its inbox sender and join handle.
    fn spawn(
        &self,
        session_key: SessionKey,
        channel: &str,
        chat_id: &str,
        semaphore: Arc<Semaphore>,
        status_indicator: Option<Arc<StatusComposer>>,
    ) -> (mpsc::Sender<ActorMessage>, JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(ACTOR_INBOX_SIZE);

        // Create a per-session proxy channel. ALL outbound messages from this
        // session (tools, final reply, errors) flow through proxy_tx. A
        // forwarding task checks whether this session is active and either
        // delivers immediately or buffers for later.
        let (proxy_tx, proxy_rx) = mpsc::channel::<OutboundMessage>(64);

        // Per-session tools — they write to proxy_tx, not the real out_tx
        let message_tool = MessageTool::with_context(proxy_tx.clone(), channel, chat_id);
        let send_file_tool = SendFileTool::with_context(proxy_tx.clone(), channel, chat_id);

        // Build per-user workspace directory for file isolation.
        // Each user's tools are restricted to their own workspace via
        // resolve_path() (application-level) and sandbox-exec SBPL (kernel-level on macOS).
        let encoded_base = crew_bus::session::encode_path_component(&session_key.base_key());
        let user_workspace = self
            .data_dir
            .join("users")
            .join(&encoded_base)
            .join("workspace");
        if let Err(e) = std::fs::create_dir_all(&user_workspace) {
            warn!(
                session = %session_key,
                path = %user_workspace.display(),
                "failed to create per-user workspace: {e}, falling back to shared cwd"
            );
        }

        // Create tool registry with cwd-bound tools pointing to the per-user workspace.
        // A fresh sandbox is created per user so the SBPL profile restricts writes
        // to this user's workspace directory (kernel-enforced on macOS).
        let user_sandbox = crew_agent::create_sandbox(&self.sandbox_config);
        let mut tools = self
            .tool_registry_factory
            .create_registry_for_workspace(&user_workspace, user_sandbox);
        tools.register(message_tool);
        tools.register(send_file_tool);

        // Spawn tool (per-session context, fully configured)
        let mut spawn_tool = SpawnTool::with_context(
            self.llm.clone(),
            self.memory.clone(),
            self.cwd.clone(),
            self.spawn_inbound_tx.clone(),
            channel,
            chat_id,
        )
        .with_provider_policy(self.provider_policy.clone());
        if let Some(ref prompt) = self.worker_prompt {
            spawn_tool = spawn_tool.with_worker_prompt(prompt.clone());
        }
        if let Some(ref router) = self.provider_router {
            spawn_tool = spawn_tool.with_provider_router(router.clone());
        }

        // Wire direct background result injection (bypasses InboundMessage relay)
        let bg_tx = tx.clone();
        spawn_tool = spawn_tool.with_background_result_sender(Arc::new(
            move |task_label: String, content: String| {
                let tx = bg_tx.clone();
                Box::pin(async move {
                    let _ = tx
                        .send(ActorMessage::BackgroundResult {
                            task_label,
                            content,
                        })
                        .await;
                })
            },
        ));

        tools.register(spawn_tool);

        // Cron tool (per-session context)
        if let Some(ref cron_service) = self.cron_service {
            let cron_tool = CronTool::with_context(cron_service.clone(), channel, chat_id);
            tools.register(cron_tool);
        }

        // Pipeline tool (if available)
        if let Some(ref pf) = self.pipeline_factory {
            let pt = pf.create();
            tools.register_arc(pt);
        }

        // Build per-session Agent
        let agent_id = AgentId::new(format!("session-{}", session_key));
        let system_prompt = self
            .system_prompt
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        let mut agent = Agent::new(agent_id, self.llm.clone(), tools, self.memory.clone())
            .with_config(self.agent_config.clone())
            .with_reporter(Arc::new(crew_agent::SilentReporter))
            .with_shutdown(self.shutdown.clone())
            .with_system_prompt(system_prompt);

        if let Some(ref embedder) = self.embedder {
            agent = agent.with_embedder(embedder.clone());
        }
        if let Some(ref hooks) = self.hooks {
            agent = agent.with_hooks(hooks.clone());
        }
        if let Some(ref ctx) = self.hook_context_template {
            agent = agent.with_hook_context(HookContext {
                session_id: Some(session_key.to_string()),
                profile_id: ctx.profile_id.clone(),
            });
        }

        // Create a per-actor SessionHandle — each actor owns its session data.
        // No shared mutex, no cross-session contention.
        let session_handle = Arc::new(Mutex::new(SessionHandle::open(
            &self.data_dir,
            &session_key,
        )));

        // Load per-user status configuration
        let user_status_config = UserStatusConfig::load(&self.data_dir, &session_key.base_key());

        let actor = SessionActor {
            session_key: session_key.clone(),
            channel: channel.to_string(),
            chat_id: chat_id.to_string(),
            inbox: rx,
            agent: Arc::new(agent),
            session_handle,
            llm_for_compaction: self.llm_for_compaction.clone(),
            out_tx: proxy_tx, // actor sends through proxy, not directly
            status_indicator,
            user_status_config,
            data_dir: self.data_dir.clone(),
            max_history: self.max_history.clone(),
            idle_timeout: self.idle_timeout,
            session_timeout: self.session_timeout,
            semaphore,
            global_shutdown: self.shutdown.clone(),
            cancelled: Arc::new(AtomicBool::new(false)),
            queue_mode: self.queue_mode,
            responsiveness: ResponsivenessObserver::new(),
            adaptive_router: self.adaptive_router.clone(),
            memory_store: self.memory_store.clone(),
            active_overflow_tasks: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        };

        // Spawn the outbound forwarding task — buffers messages from inactive sessions
        let fwd_session_key = session_key.clone();
        let fwd_out_tx = self.out_tx.clone();
        let fwd_active = self.active_sessions.clone();
        let fwd_pending = self.pending_messages.clone();
        let fwd_channel = channel.to_string();
        let fwd_chat_id = chat_id.to_string();
        tokio::spawn(outbound_forwarder(
            proxy_rx,
            fwd_out_tx,
            fwd_session_key,
            fwd_channel,
            fwd_chat_id,
            fwd_active,
            fwd_pending,
        ));

        let join_handle = tokio::spawn(actor.run());

        info!(session = %session_key, channel, chat_id, "spawned session actor");
        (tx, join_handle)
    }
}

/// Forwarding task: reads from the session's proxy channel and either delivers
/// messages directly (if this session is active) or buffers them.
async fn outbound_forwarder(
    mut proxy_rx: mpsc::Receiver<OutboundMessage>,
    out_tx: mpsc::Sender<OutboundMessage>,
    session_key: SessionKey,
    channel: String,
    chat_id: String,
    active_sessions: Arc<Mutex<ActiveSessionStore>>,
    pending_messages: PendingMessages,
) {
    let my_topic = session_key.topic().unwrap_or("").to_string();
    let base_key = session_key.base_key().to_string();
    let key_str = session_key.to_string();

    while let Some(msg) = proxy_rx.recv().await {
        let active_topic = active_sessions
            .lock()
            .await
            .get_active_topic(&base_key)
            .to_string();

        if my_topic == active_topic {
            // Session is active — deliver immediately
            let _ = out_tx.send(msg).await;
        } else {
            // Session is inactive — buffer the message
            let mut pending = pending_messages.lock().await;
            let buf = pending.entry(key_str.clone()).or_default();
            let is_first = buf.is_empty();
            if buf.len() < MAX_PENDING_PER_SESSION {
                buf.push(msg);
            } else {
                warn!(session = %session_key, "pending buffer full, dropping message");
            }
            drop(pending); // release lock before sending notification

            if is_first {
                let topic_label = if my_topic.is_empty() {
                    "(default)"
                } else {
                    &my_topic
                };
                let _ = out_tx
                    .send(OutboundMessage {
                        channel: channel.clone(),
                        chat_id: chat_id.clone(),
                        content: format!("📌 {topic_label} finished. /s {topic_label} to view."),
                        reply_to: None,
                        media: vec![],
                        metadata: serde_json::json!({}),
                    })
                    .await;
            }
        }
    }
}

// ── SessionActor ────────────────────────────────────────────────────────────

/// Long-lived task that processes all messages for one session.
struct SessionActor {
    session_key: SessionKey,
    channel: String,
    chat_id: String,

    inbox: mpsc::Receiver<ActorMessage>,

    agent: Arc<Agent>,

    /// Per-actor session handle — owns this session's data, no shared mutex.
    session_handle: Arc<Mutex<SessionHandle>>,
    llm_for_compaction: Arc<dyn LlmProvider>,

    out_tx: mpsc::Sender<OutboundMessage>,

    status_indicator: Option<Arc<StatusComposer>>,
    /// Per-user status configuration (greeting, visibility toggles, custom layers).
    user_status_config: UserStatusConfig,
    /// Data directory for persisting user configs.
    data_dir: std::path::PathBuf,
    max_history: Arc<std::sync::atomic::AtomicUsize>,

    idle_timeout: Duration,
    session_timeout: Duration,
    semaphore: Arc<Semaphore>,
    /// Global shutdown flag (Ctrl+C, etc.)
    global_shutdown: Arc<AtomicBool>,
    /// Per-actor cancellation flag (only affects this session)
    cancelled: Arc<AtomicBool>,
    /// Queue mode for handling messages that arrive during active processing.
    queue_mode: QueueMode,
    /// Tracks LLM response latencies and detects sustained degradation.
    responsiveness: ResponsivenessObserver,
    /// Side-channel to AdaptiveRouter for toggling auto-protection.
    adaptive_router: Option<Arc<AdaptiveRouter>>,
    /// Memory store for saving long research reports out-of-band.
    memory_store: Option<Arc<MemoryStore>>,
    /// Active overflow task counter for concurrency limiting.
    active_overflow_tasks: Arc<std::sync::atomic::AtomicU32>,
}

impl SessionActor {
    async fn run(mut self) {
        loop {
            tokio::select! {
                msg = self.inbox.recv() => {
                    match msg {
                        Some(ActorMessage::Inbound { message, image_media }) => {
                            // Check for abort trigger before processing
                            if crew_core::is_abort_trigger(&message.content) {
                                debug!(session = %self.session_key, "abort trigger detected");
                                self.cancelled.store(true, Ordering::Release);
                                let _ = self.out_tx.send(OutboundMessage {
                                    channel: self.channel.clone(),
                                    chat_id: self.chat_id.clone(),
                                    content: "🛑 Cancelled.".to_string(),
                                    reply_to: None,
                                    media: vec![],
                                    metadata: serde_json::json!({}),
                                }).await;
                                // Reset for next message
                                self.cancelled.store(false, Ordering::Release);
                                continue;
                            }

                            // Handle slash commands (no LLM round-trip)
                            if self.try_handle_command(&message).await {
                                continue;
                            }

                            // Drain any queued messages according to queue mode
                            let (final_message, final_media) =
                                self.drain_queue(message, image_media).await;

                            // In speculative mode, detect slow LLM calls and
                            // spawn concurrent agent tasks for overflow messages.
                            if self.queue_mode == QueueMode::Speculative {
                                self.process_inbound_speculative(final_message, final_media).await;
                            } else {
                                self.process_inbound(final_message, final_media).await;
                            }
                        }
                        Some(ActorMessage::BackgroundResult { task_label, content }) => {
                            self.inject_background_result(&task_label, &content).await;
                        }
                        Some(ActorMessage::Cancel) => {
                            debug!(session = %self.session_key, "cancel requested");
                            self.cancelled.store(true, Ordering::Release);
                        }
                        None => {
                            // All senders dropped
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep(self.idle_timeout) => {
                    debug!(session = %self.session_key, "idle timeout, shutting down actor");
                    break;
                }
            }

            if self.global_shutdown.load(Ordering::Acquire)
                || self.cancelled.load(Ordering::Acquire)
            {
                break;
            }
        }

        debug!(session = %self.session_key, "actor exiting");
    }

    /// Handle slash commands that don't need an LLM round-trip.
    /// Returns `true` if the message was consumed as a command.
    async fn try_handle_command(&mut self, message: &InboundMessage) -> bool {
        let text = message.content.trim();
        if !text.starts_with('/') {
            return false;
        }

        let parts: Vec<&str> = text.split_whitespace().collect();
        let cmd = parts[0];

        match cmd {
            "/adaptive" => {
                self.handle_adaptive_command(&parts[1..]).await;
                true
            }
            "/queue" => {
                self.handle_queue_command(&parts[1..]).await;
                true
            }
            "/status" => {
                self.handle_status_command(&parts[1..]).await;
                true
            }
            _ => false, // Unknown slash command — pass through to LLM
        }
    }

    /// `/adaptive` — view or toggle adaptive routing features.
    ///
    /// Usage:
    ///   /adaptive                       — show current status
    ///   /adaptive circuit on|off        — toggle auto circuit breaker
    ///   /adaptive lane on|off           — toggle lane changing
    ///   /adaptive qos on|off            — toggle QoS ranking
    async fn handle_adaptive_command(&self, args: &[&str]) {
        let Some(ref router) = self.adaptive_router else {
            self.send_reply("Adaptive routing is not enabled.").await;
            return;
        };

        if args.is_empty() {
            // Show status
            let status = router.adaptive_status();
            let provider = router.current_provider_name();
            let snapshots = router.metrics_snapshots();

            let mut lines = vec![
                "**Adaptive Routing**".to_string(),
                format!("  mode:        {}", status.mode),
                format!(
                    "  qos ranking: {}",
                    if status.qos_ranking { "on" } else { "off" }
                ),
                format!("  current:     {provider}"),
            ];

            if !snapshots.is_empty() {
                lines.push(String::new());
                lines.push("**Providers**".to_string());
                for (name, model, snap) in &snapshots {
                    lines.push(format!(
                        "  {name} ({model}): latency={:.0}ms ok={} err={} {}",
                        snap.latency_ema_ms,
                        snap.success_count,
                        snap.failure_count,
                        if snap.consecutive_failures >= status.failure_threshold {
                            "⛔ OPEN"
                        } else {
                            "✅"
                        },
                    ));
                }
            }

            self.send_reply(&lines.join("\n")).await;
            return;
        }

        match args[0] {
            // Mode switching: /adaptive off|hedge|lane
            "off" => {
                router.set_mode(AdaptiveMode::Off);
                self.send_reply("Adaptive mode: off (static priority, failover only)")
                    .await;
            }
            "hedge" | "race" | "circuit" => {
                router.set_mode(AdaptiveMode::Hedge);
                let status = router.adaptive_status();
                if status.provider_count < 2 {
                    self.send_reply("Adaptive mode: hedge (race 2 providers, take winner)\n⚠️ Only 1 provider configured — hedge needs ≥2 to race. Currently behaves like off mode.").await;
                } else {
                    self.send_reply(&format!(
                        "Adaptive mode: hedge (race 2 of {} providers, take winner)",
                        status.provider_count
                    ))
                    .await;
                }
            }
            "lane" => {
                router.set_mode(AdaptiveMode::Lane);
                let status = router.adaptive_status();
                if status.provider_count < 2 {
                    self.send_reply("Adaptive mode: lane (score-based provider selection)\n⚠️ Only 1 provider configured — lane needs ≥2 to compare. Currently behaves like off mode.").await;
                } else {
                    self.send_reply(&format!(
                        "Adaptive mode: lane (score-based selection across {} providers)",
                        status.provider_count
                    ))
                    .await;
                }
            }
            // QoS toggle: /adaptive qos [on|off]
            "qos" => {
                if let Some(value) = args.get(1) {
                    let enabled = match *value {
                        "on" | "true" | "1" => true,
                        "off" | "false" | "0" => false,
                        other => {
                            self.send_reply(&format!("Invalid value: {other}. Use: on/off"))
                                .await;
                            return;
                        }
                    };
                    router.set_qos_ranking(enabled);
                    self.send_reply(&format!(
                        "QoS ranking: {}",
                        if enabled { "on" } else { "off" }
                    ))
                    .await;
                } else {
                    let on = router.adaptive_status().qos_ranking;
                    self.send_reply(&format!("QoS ranking: {}", if on { "on" } else { "off" }))
                        .await;
                }
            }
            other => {
                self.send_reply(&format!(
                    "Unknown option: {other}\nUsage: /adaptive [off|hedge|lane|qos [on|off]]"
                ))
                .await;
            }
        }
    }

    /// `/queue` — view or change the queue mode.
    ///
    /// Usage:
    ///   /queue                          — show current mode
    ///   /queue followup|collect|steer|interrupt
    async fn handle_queue_command(&mut self, args: &[&str]) {
        if args.is_empty() {
            self.send_reply(&format!("Queue mode: {:?}", self.queue_mode))
                .await;
            return;
        }

        let mode = match args[0] {
            "followup" => QueueMode::Followup,
            "collect" => QueueMode::Collect,
            "steer" => QueueMode::Steer,
            "interrupt" => QueueMode::Interrupt,
            "spec" | "speculative" => QueueMode::Speculative,
            other => {
                self.send_reply(&format!(
                    "Unknown mode: {other}. Use: followup, collect, steer, interrupt, spec"
                ))
                .await;
                return;
            }
        };

        self.queue_mode = mode;
        self.send_reply(&format!("Queue mode set to: {:?}", mode))
            .await;
    }

    /// `/status` — view or configure per-user status layers.
    ///
    /// Usage:
    ///   /status                        — show current config
    ///   /status greeting <text>        — set greeting template
    ///   /status provider on|off        — toggle provider layer
    ///   /status metrics on|off         — toggle metrics layer
    ///   /status words <w1,w2,...>       — set custom status words
    ///   /status add <id> <priority> <text> — add custom layer
    ///   /status remove <id>            — remove custom layer
    ///   /status reset                  — reset to defaults
    async fn handle_status_command(&mut self, args: &[&str]) {
        use crate::status_layers::{CustomLayerDef, LayerPolicy};

        if args.is_empty() {
            let cfg = &self.user_status_config;
            let mut lines = vec![
                "**Status Config**".to_string(),
                format!(
                    "Greeting: {}",
                    cfg.greeting_template.as_deref().unwrap_or("(none)")
                ),
                format!("Provider visible: {}", cfg.provider_visible),
                format!("Metrics visible: {}", cfg.metrics_visible),
                format!("Greeting duration: {}s", cfg.greeting_duration_secs),
            ];
            if let Some(ref words) = cfg.status_words {
                lines.push(format!("Words: {}", words.join(", ")));
            }
            if let Some(ref locale) = cfg.locale {
                lines.push(format!("Locale: {locale}"));
            }
            for custom in &cfg.custom_layers {
                lines.push(format!(
                    "Custom layer `{}` (p={}): {}",
                    custom.id, custom.priority, custom.content
                ));
            }
            self.send_reply(&lines.join("\n")).await;
            return;
        }

        match args[0] {
            "greeting" => {
                if args.len() < 2 {
                    self.send_reply("Usage: /status greeting <text>  (or /status greeting off)")
                        .await;
                    return;
                }
                let text = args[1..].join(" ");
                if text == "off" || text == "none" {
                    self.user_status_config.greeting_template = None;
                    self.send_reply("Greeting disabled.").await;
                } else {
                    self.user_status_config.greeting_template = Some(text.clone());
                    self.send_reply(&format!("Greeting set: {text}")).await;
                }
            }
            "provider" => {
                let on = match args.get(1).copied() {
                    Some("on" | "true" | "1") => true,
                    Some("off" | "false" | "0") => false,
                    _ => {
                        self.send_reply("Usage: /status provider on|off").await;
                        return;
                    }
                };
                self.user_status_config.provider_visible = on;
                self.send_reply(&format!(
                    "Provider layer: {}",
                    if on { "visible" } else { "hidden" }
                ))
                .await;
            }
            "metrics" => {
                let on = match args.get(1).copied() {
                    Some("on" | "true" | "1") => true,
                    Some("off" | "false" | "0") => false,
                    _ => {
                        self.send_reply("Usage: /status metrics on|off").await;
                        return;
                    }
                };
                self.user_status_config.metrics_visible = on;
                self.send_reply(&format!(
                    "Metrics layer: {}",
                    if on { "visible" } else { "hidden" }
                ))
                .await;
            }
            "words" => {
                if args.len() < 2 {
                    self.send_reply("Usage: /status words word1,word2,...")
                        .await;
                    return;
                }
                let words: Vec<String> = args[1..]
                    .join(" ")
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if words.is_empty() {
                    self.user_status_config.status_words = None;
                    self.send_reply("Status words reset to default.").await;
                } else {
                    let preview = words.join(", ");
                    self.user_status_config.status_words = Some(words);
                    self.send_reply(&format!("Status words: {preview}")).await;
                }
            }
            "add" => {
                // /status add <id> <priority> <text>
                if args.len() < 4 {
                    self.send_reply("Usage: /status add <id> <priority> <text>")
                        .await;
                    return;
                }
                let id = args[1].to_string();
                let priority: u8 = match args[2].parse() {
                    Ok(p) => p,
                    Err(_) => {
                        self.send_reply("Priority must be a number 0-255.").await;
                        return;
                    }
                };
                let content = args[3..].join(" ");
                // Remove existing layer with same ID
                self.user_status_config.custom_layers.retain(|l| l.id != id);
                self.user_status_config.custom_layers.push(CustomLayerDef {
                    id: id.clone(),
                    priority,
                    policy: LayerPolicy::Fixed,
                    content: content.clone(),
                });
                self.send_reply(&format!("Added layer `{id}` (p={priority}): {content}"))
                    .await;
            }
            "remove" => {
                if args.len() < 2 {
                    self.send_reply("Usage: /status remove <id>").await;
                    return;
                }
                let id = args[1];
                let before = self.user_status_config.custom_layers.len();
                self.user_status_config.custom_layers.retain(|l| l.id != id);
                if self.user_status_config.custom_layers.len() < before {
                    self.send_reply(&format!("Removed layer `{id}`.")).await;
                } else {
                    self.send_reply(&format!("No custom layer `{id}` found."))
                        .await;
                }
            }
            "duration" => {
                if let Some(secs) = args.get(1).and_then(|s| s.parse::<u64>().ok()) {
                    self.user_status_config.greeting_duration_secs = secs;
                    self.send_reply(&format!("Greeting duration: {secs}s"))
                        .await;
                } else {
                    self.send_reply("Usage: /status duration <seconds>").await;
                    return;
                }
            }
            "locale" => {
                if let Some(loc) = args.get(1) {
                    if *loc == "auto" || *loc == "off" {
                        self.user_status_config.locale = None;
                        self.send_reply("Locale: auto-detect").await;
                    } else {
                        self.user_status_config.locale = Some(loc.to_string());
                        self.send_reply(&format!("Locale: {loc}")).await;
                    }
                } else {
                    self.send_reply("Usage: /status locale <en|zh|auto>").await;
                    return;
                }
            }
            "reset" => {
                self.user_status_config = UserStatusConfig::default();
                self.send_reply("Status config reset to defaults.").await;
            }
            other => {
                self.send_reply(&format!(
                    "Unknown status subcommand: {other}\n\
                    Usage: /status [greeting|provider|metrics|words|add|remove|duration|locale|reset]"
                )).await;
                return;
            }
        }

        // Persist changes
        let base_key = self.session_key.base_key();
        if let Err(e) = self.user_status_config.save(&self.data_dir, &base_key) {
            warn!(error = %e, "failed to save user status config");
        }
    }

    /// Send a short reply to the user (for command responses).
    async fn send_reply(&self, content: &str) {
        let _ = self
            .out_tx
            .send(OutboundMessage {
                channel: self.channel.clone(),
                chat_id: self.chat_id.clone(),
                content: content.to_string(),
                reply_to: None,
                media: vec![],
                metadata: serde_json::json!({}),
            })
            .await;
    }

    /// Drain any already-queued messages from the inbox and combine them
    /// with the current message according to the configured queue mode.
    ///
    /// - Followup: return the message as-is (queued messages processed next iteration)
    /// - Collect: batch all queued messages into one combined prompt
    /// - Steer: discard current message, use the newest queued message instead
    /// - Interrupt: same as Steer (cancellation already handled at dispatch level)
    async fn drain_queue(
        &mut self,
        message: InboundMessage,
        image_media: Vec<String>,
    ) -> (InboundMessage, Vec<String>) {
        match self.queue_mode {
            QueueMode::Followup | QueueMode::Speculative => (message, image_media),
            QueueMode::Collect => {
                let mut combined_content = message.content.clone();
                let mut combined_media = image_media;
                let mut count = 0u32;

                // Non-blocking drain of queued inbound messages
                loop {
                    match self.inbox.try_recv() {
                        Ok(ActorMessage::Inbound {
                            message: queued,
                            image_media: queued_media,
                        }) => {
                            if crew_core::is_abort_trigger(&queued.content) {
                                debug!(session = %self.session_key, "abort in queue, cancelling batch");
                                self.cancelled.store(true, Ordering::Release);
                                break;
                            }
                            count += 1;
                            combined_content
                                .push_str(&format!("\n---\nQueued #{count}: {}", queued.content));
                            combined_media.extend(queued_media);
                        }
                        Ok(ActorMessage::BackgroundResult {
                            task_label,
                            content,
                        }) => {
                            self.inject_background_result(&task_label, &content).await;
                        }
                        Ok(ActorMessage::Cancel) => {
                            self.cancelled.store(true, Ordering::Release);
                            break;
                        }
                        Err(_) => break, // inbox empty
                    }
                }
                let mut msg = message;
                msg.content = combined_content;
                (msg, combined_media)
            }
            QueueMode::Steer | QueueMode::Interrupt => {
                let mut latest_message = message;
                let mut latest_media = image_media;

                // Non-blocking drain: keep only the newest inbound message
                loop {
                    match self.inbox.try_recv() {
                        Ok(ActorMessage::Inbound {
                            message: queued,
                            image_media: queued_media,
                        }) => {
                            if crew_core::is_abort_trigger(&queued.content) {
                                debug!(session = %self.session_key, "abort in queue, cancelling");
                                self.cancelled.store(true, Ordering::Release);
                                break;
                            }
                            debug!(session = %self.session_key, "steer: replacing with newer message");
                            latest_message = queued;
                            latest_media = queued_media;
                        }
                        Ok(ActorMessage::BackgroundResult {
                            task_label,
                            content,
                        }) => {
                            self.inject_background_result(&task_label, &content).await;
                        }
                        Ok(ActorMessage::Cancel) => {
                            self.cancelled.store(true, Ordering::Release);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                (latest_message, latest_media)
            }
        }
    }

    /// Inject a background task result into the conversation.
    ///
    /// For long results (>1000 chars), the full content is saved to the memory
    /// bank and only a summary is injected into session context.  The agent can
    /// retrieve the full report via `recall_memory("<slug>")`.
    async fn inject_background_result(&self, task_label: &str, content: &str) {
        const SUMMARY_THRESHOLD: usize = 1000;
        const SUMMARY_CHARS: usize = 800;

        let (context_content, notification) = if content.len() > SUMMARY_THRESHOLD {
            // Save full report to memory bank
            let slug = task_label
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || c == '-' {
                        c
                    } else {
                        '-'
                    }
                })
                .collect::<String>()
                .to_lowercase();
            let slug = slug.trim_matches('-').to_string();

            if let Some(ref ms) = self.memory_store {
                let report_md = format!(
                    "# {task_label}\n\n_Generated: {}_\n\n{content}",
                    chrono::Utc::now().format("%Y-%m-%d %H:%M UTC"),
                );
                if let Err(e) = ms.write_entity(&slug, &report_md).await {
                    warn!(session = %self.session_key, error = %e, "failed to save report to memory bank");
                } else {
                    info!(session = %self.session_key, slug = %slug, len = content.len(), "saved report to memory bank");
                }
            }

            // Truncate for context injection
            let summary: String = content.chars().take(SUMMARY_CHARS).collect();
            let ctx = format!(
                "[Background task \"{task_label}\" completed]\n\n\
                 {summary}\n\n[... truncated — full report ({} chars) saved to memory bank as \"{slug}\". \
                 Use recall_memory(\"{slug}\") to load the complete report.]",
                content.len(),
            );

            // Notification includes a preview for the user
            let preview: String = content.chars().take(300).collect();
            let notif = format!(
                "✅ **{task_label}** completed.\n\n{preview}...\n\n_Full report saved. Ask me to recall it for details._",
            );

            (ctx, notif)
        } else {
            // Short result — inject fully
            let ctx = format!("[Background task \"{task_label}\" completed]\n\n{content}");
            let notif = format!("✅ **{task_label}** completed.\n\n{content}");
            (ctx, notif)
        };

        let system_msg = Message::system(context_content);
        {
            let mut handle = self.session_handle.lock().await;
            if let Err(e) = handle.add_message(system_msg).await {
                warn!(session = %self.session_key, error = %e, "failed to inject background result");
            }
        }

        // Notify user with preview
        let _ = self
            .out_tx
            .send(OutboundMessage {
                channel: self.channel.clone(),
                chat_id: self.chat_id.clone(),
                content: notification,
                reply_to: None,
                media: vec![],
                metadata: serde_json::json!({}),
            })
            .await;
    }

    /// Speculative processing: runs the LLM call but monitors the inbox.
    /// If the call exceeds 2× responsiveness baseline and a new user message
    /// arrives, the new message gets a quick LLM response via the adaptive
    /// router (no tools, lightweight) while the original call continues.
    /// Both results are delivered to the user.
    async fn process_inbound_speculative(
        &mut self,
        inbound: InboundMessage,
        image_media: Vec<String>,
    ) {
        // Capture the platform message ID for reply threading
        let inbound_message_id = inbound.message_id.clone();

        let patience = self
            .responsiveness
            .baseline()
            .map(|b| (b * 2).max(Duration::from_secs(10)))
            .unwrap_or(Duration::from_secs(30));
        debug!(
            session = %self.session_key,
            patience_ms = patience.as_millis(),
            baseline_ms = ?self.responsiveness.baseline().map(|b| b.as_millis()),
            samples = self.responsiveness.sample_count(),
            "speculative: entering concurrent processing"
        );

        // ── Setup (needs &mut self briefly for permit + reporter) ────────

        let _permit = match self.semaphore.acquire().await {
            Ok(p) => p,
            Err(_) => return,
        };

        let max_history = self.max_history.load(Ordering::Acquire);

        // Save the primary user message to session history BEFORE spawning
        // so overflow reads see it in context (chronological ordering).
        let user_msg = Message {
            role: MessageRole::User,
            content: if inbound.content.is_empty() && !image_media.is_empty() {
                "[User sent an image]".to_string()
            } else {
                inbound.content.clone()
            },
            media: image_media.clone(),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        };
        {
            let mut handle = self.session_handle.lock().await;
            // Auto-generate summary from first user message
            {
                let session = handle.get_or_create();
                if session.summary.is_none() && !inbound.content.trim().is_empty() {
                    let summary: String = inbound.content.chars().take(100).collect();
                    session.summary = Some(summary);
                }
            }
            let _ = handle.add_message(user_msg).await;
        }

        // Get conversation history (now includes the user message we just saved)
        let history: Vec<Message> = {
            let handle = self.session_handle.lock().await;
            handle.get_history(max_history).to_vec()
        };

        // Token tracker for status indicator
        let token_tracker = Arc::new(TokenTracker::new());

        // Start status indicator
        let status_handle = self.status_indicator.as_ref().map(|si| {
            let voice_transcript = inbound
                .metadata
                .get("voice_transcript")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            si.start(
                self.chat_id.clone(),
                &inbound.content,
                Arc::clone(&token_tracker),
                voice_transcript,
                &self.user_status_config,
            )
        });

        // Set up progressive streaming reporter
        let (stream_tx, stream_rx) = tokio::sync::mpsc::unbounded_channel();
        let reporter = Arc::new(crate::stream_reporter::ChannelStreamReporter::new(
            stream_tx.clone(),
        ));
        self.agent.set_reporter(reporter);

        // Wire adaptive router status callback to forward through the stream channel.
        // This lets failover events inside chat_stream() surface as LlmStatus messages.
        if let Some(ref router) = self.adaptive_router {
            let status_tx = stream_tx.clone();
            router.set_status_callback(Some(Arc::new(move |message: String| {
                let _ = status_tx
                    .send(crate::stream_reporter::StreamProgressEvent::LlmStatus { message });
            })));
        }

        // Set provider layer on the status composer
        if let Some(ref handle) = status_handle {
            handle.set_provider(self.agent.provider_name(), self.agent.model_id());
        }

        // Spawn stream forwarder task (only for channels that support editing)
        let stream_forwarder = if let Some(ref si) = self.status_indicator {
            let channel = Arc::clone(si.channel());
            if channel.supports_edit() {
                let cancel_status = status_handle.as_ref().map(|h| Arc::clone(&h.cancelled));
                let status_msg_id = status_handle.as_ref().map(|h| Arc::clone(&h.status_msg_id));
                Some(tokio::spawn(crate::stream_reporter::run_stream_forwarder(
                    stream_rx,
                    channel,
                    self.chat_id.clone(),
                    cancel_status,
                    status_msg_id,
                )))
            } else {
                drop(stream_rx);
                None
            }
        } else {
            drop(stream_rx);
            None
        };

        // ── Spawn agent call as a separate task (Arc<Agent>, no &mut self) ──

        let agent = Arc::clone(&self.agent);
        let content = inbound.content.clone();
        let media = image_media;
        let tracker = Arc::clone(&token_tracker);
        let session_timeout = self.session_timeout;

        // The agent receives the history snapshot (which includes the user
        // message we saved above). The agent will prepend its own system
        // prompt and user message internally — we'll deduplicate on save.
        // Note: we pass the history WITHOUT the user message we just saved,
        // because process_message_tracked adds a user message itself.
        // The pre-saved user message ensures overflow calls see it in history.
        let history_for_agent: Vec<Message> = if !history.is_empty() {
            // Strip the last message (the user msg we just saved) since the
            // agent's process_message_inner will re-add it.
            history[..history.len() - 1].to_vec()
        } else {
            vec![]
        };

        // Snapshot for overflow tasks: conversation context BEFORE the
        // primary task, EXCLUDING the primary user message.  Overflow needs
        // identity, preferences, and prior exchanges, but must NOT see the
        // primary question — otherwise the LLM re-answers it alongside the
        // overflow question.  Same base as history_for_agent (primary user
        // message stripped).
        let overflow_history = history_for_agent.clone();

        let mut agent_task = tokio::spawn(async move {
            let start = Instant::now();
            let result = tokio::time::timeout(
                session_timeout,
                agent.process_message_tracked(&content, &history_for_agent, media, &tracker),
            )
            .await;
            (result, start.elapsed())
        });

        // ── Select loop: poll inbox while agent runs ────────────────────

        let started = Instant::now();
        let mut overflow_served = false;
        let mut overflow_commands: Vec<InboundMessage> = Vec::new();

        let (agent_result, llm_latency) = loop {
            tokio::select! {
                // Agent task completed
                join_result = &mut agent_task => {
                    match join_result {
                        Ok(pair) => break pair,
                        Err(e) => {
                            warn!(session = %self.session_key, error = %e, "agent task panicked");
                            self.send_reply("Internal error during processing.").await;
                            // Clean up reporter + status + callback
                            self.agent.set_reporter(Arc::new(crew_agent::SilentReporter));
                            if let Some(ref router) = self.adaptive_router {
                                router.set_status_callback(None);
                            }
                            if let Some(handle) = status_handle {
                                handle.stop().await;
                            }
                            return;
                        }
                    }
                }
                // New message arrived in inbox
                msg = self.inbox.recv() => {
                    match msg {
                        Some(ActorMessage::Inbound { message, image_media: _ }) => {
                            if crew_core::is_abort_trigger(&message.content) {
                                self.cancelled.store(true, Ordering::Release);
                                self.send_reply("🛑 Cancelled.").await;
                                continue;
                            }
                            // Check if this is a slash command — handle inline
                            // instead of spawning an overflow agent.
                            if message.content.trim().starts_with('/') {
                                overflow_commands.push(message);
                                continue;
                            }
                            let elapsed = started.elapsed();
                            info!(
                                session = %self.session_key,
                                elapsed_ms = elapsed.as_millis(),
                                patience_ms = patience.as_millis(),
                                "speculative: serving overflow message"
                            );
                            // Always spawn — the user sent a new message while
                            // the primary is running, so it needs processing.
                            self.serve_overflow(&message, &overflow_history);
                            overflow_served = true;
                        }
                        Some(ActorMessage::BackgroundResult { task_label, content }) => {
                            self.inject_background_result(&task_label, &content).await;
                        }
                        Some(ActorMessage::Cancel) => {
                            self.cancelled.store(true, Ordering::Release);
                        }
                        None => {
                            // All senders dropped — actor shutting down
                            self.agent.set_reporter(Arc::new(crew_agent::SilentReporter));
                            if let Some(ref router) = self.adaptive_router {
                                router.set_status_callback(None);
                            }
                            if let Some(handle) = status_handle {
                                handle.stop().await;
                            }
                            return;
                        }
                    }
                }
            }
        };

        // ── Post-processing (back to &mut self) ────────────────────────

        // Drop the semaphore permit before &mut self operations below.
        drop(_permit);

        // Handle any slash commands that arrived during the select loop.
        // We deferred them to avoid &mut self borrow conflicts in tokio::select!.
        for cmd_msg in overflow_commands {
            self.try_handle_command(&cmd_msg).await;
        }

        // Feed latency to responsiveness observer
        self.responsiveness.record(llm_latency);
        if self.responsiveness.should_activate() {
            warn!(
                session = %self.session_key,
                baseline_ms = ?self.responsiveness.baseline().map(|b| b.as_millis()),
                latency_ms = llm_latency.as_millis(),
                consecutive_slow = self.responsiveness.consecutive_slow_count(),
                "sustained latency degradation detected, activating auto-protection"
            );
            self.responsiveness.set_active(true);
            self.queue_mode = QueueMode::Speculative;
            if let Some(ref router) = self.adaptive_router {
                router.set_mode(AdaptiveMode::Hedge);
                let _ = self.out_tx.send(OutboundMessage {
                    channel: self.channel.clone(),
                    chat_id: self.chat_id.clone(),
                    content: "⚡ Detected slow responses. Enabling hedge racing + speculative queue — you won't be blocked.".to_string(),
                    reply_to: None,
                    media: vec![],
                    metadata: serde_json::json!({}),
                }).await;
            }
        } else if self.responsiveness.should_deactivate() {
            info!(session = %self.session_key, "provider recovered, reverting to normal mode");
            self.responsiveness.set_active(false);
            self.queue_mode = QueueMode::Followup;
            if let Some(ref router) = self.adaptive_router {
                router.set_mode(AdaptiveMode::Off);
            }
        }

        // Reset reporter to silent (drops stream_tx → forwarder finishes)
        self.agent
            .set_reporter(Arc::new(crew_agent::SilentReporter));

        // Clear adaptive router status callback (stream_tx is being dropped)
        if let Some(ref router) = self.adaptive_router {
            router.set_status_callback(None);
        }

        // Wait for stream forwarder
        let stream_result = if let Some(handle) = stream_forwarder {
            (handle.await).ok()
        } else {
            None
        };

        // Stop status indicator
        if let Some(handle) = status_handle {
            handle.stop().await;
        }

        // Handle agent result — save messages (skipping user msg, already saved)
        // and send reply
        match agent_result {
            Ok(Ok(conv_response)) => {
                // Save tool calls, tool results, and assistant reply to history.
                // Skip the first message (user msg) — we already saved it before
                // spawning to maintain chronological ordering.
                {
                    let mut handle = self.session_handle.lock().await;
                    let messages_to_save = if !conv_response.messages.is_empty()
                        && conv_response.messages[0].role == MessageRole::User
                    {
                        &conv_response.messages[1..]
                    } else {
                        &conv_response.messages
                    };
                    for msg in messages_to_save {
                        if let Err(e) = handle.add_message(msg.clone()).await {
                            warn!(session = %self.session_key, role = ?msg.role, error = %e, "failed to persist message");
                        }
                    }

                    // Sort messages by timestamp to restore chronological order.
                    // During concurrent speculative overflow, overflow responses
                    // may have been inserted before the primary call's messages.
                    handle.sort_by_timestamp();
                    if let Err(e) = handle.rewrite().await {
                        warn!(session = %self.session_key, error = %e, "failed to rewrite session after sort");
                    }

                    // Compact if needed
                    if let Err(e) = crate::compaction::maybe_compact_handle(
                        &mut handle,
                        &*self.llm_for_compaction,
                    )
                    .await
                    {
                        warn!("session compaction failed: {e}");
                    }
                }

                // Send reply
                let content = strip_think_tags(&conv_response.content);
                let is_cron = inbound.channel == "system" && inbound.sender_id == "cron";
                let is_silent = content.trim().is_empty()
                    || content.contains("[SILENT]")
                    || content.contains("[NO_CHANGE]");

                if !(is_cron && is_silent) {
                    let display_content = if content.trim().is_empty() && !is_cron {
                        tracing::warn!(session = %self.session_key, "LLM returned empty content, sending fallback");
                        "(The model returned an empty response. Please try again.)".to_string()
                    } else {
                        content
                            .trim_start()
                            .strip_prefix("[SILENT]")
                            .or_else(|| content.trim_start().strip_prefix("[NO_CHANGE]"))
                            .unwrap_or(&content)
                            .to_string()
                    };

                    // If overflow was served while this task ran, prepend a
                    // marker so the user knows this is a delayed result.
                    let display_content = if overflow_served {
                        format!("⬆️ Earlier task completed:\n\n{display_content}")
                    } else {
                        display_content
                    };

                    let streamed = if let Some(ref sr) = stream_result {
                        if let Some(ref mid) = sr.message_id {
                            if let Some(ref si) = self.status_indicator {
                                let _ = si
                                    .channel()
                                    .edit_message(&self.chat_id, mid, &display_content)
                                    .await;
                            }
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };

                    if !streamed {
                        let _ = self
                            .out_tx
                            .send(OutboundMessage {
                                channel: self.channel.clone(),
                                chat_id: self.chat_id.clone(),
                                content: display_content,
                                reply_to: inbound_message_id.clone(),
                                media: vec![],
                                metadata: serde_json::json!({}),
                            })
                            .await;
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::error!(session = %self.session_key, error = %e, "agent processing failed");
                let _ = self
                    .out_tx
                    .send(OutboundMessage {
                        channel: self.channel.clone(),
                        chat_id: self.chat_id.clone(),
                        content: format!("Error: {e}"),
                        reply_to: inbound_message_id.clone(),
                        media: vec![],
                        metadata: serde_json::json!({}),
                    })
                    .await;
            }
            Err(_) => {
                tracing::error!(session = %self.session_key, "session processing timed out");
                let _ = self
                    .out_tx
                    .send(OutboundMessage {
                        channel: self.channel.clone(),
                        chat_id: self.chat_id.clone(),
                        content: "Processing timed out. Please try again.".to_string(),
                        reply_to: inbound_message_id.clone(),
                        media: vec![],
                        metadata: serde_json::json!({}),
                    })
                    .await;
            }
        }

        // Send completion marker so the API channel can close the SSE stream.
        if self.channel == "api" {
            let _ = self
                .out_tx
                .send(OutboundMessage {
                    channel: self.channel.clone(),
                    chat_id: self.chat_id.clone(),
                    content: String::new(),
                    reply_to: None,
                    media: vec![],
                    metadata: serde_json::json!({"_completion": true}),
                })
                .await;
        }
    }

    /// Spawn a full agent task for an overflow message (with tools).
    /// The task runs concurrently with the primary agent call.
    /// Each overflow gets its own chat bubble (stream reporter + status
    /// indicator) so the user sees independent progress per message.
    fn serve_overflow(&self, msg: &InboundMessage, pre_primary_history: &[Message]) {
        // Check per-session overflow concurrency limit
        let current = self.active_overflow_tasks.load(Ordering::Acquire);
        if current >= MAX_OVERFLOW_TASKS {
            warn!(
                session = %self.session_key,
                active = current,
                limit = MAX_OVERFLOW_TASKS,
                "overflow concurrency limit reached, returning busy response"
            );
            let out_tx = self.out_tx.clone();
            let channel = self.channel.clone();
            let chat_id = self.chat_id.clone();
            let reply_to = msg.message_id.clone();
            tokio::spawn(async move {
                let _ = out_tx
                    .send(OutboundMessage {
                        channel,
                        chat_id,
                        content: "I'm currently handling several tasks. Please wait a moment and try again.".to_string(),
                        reply_to,
                        media: vec![],
                        metadata: serde_json::json!({}),
                    })
                    .await;
            });
            return;
        }
        self.active_overflow_tasks.fetch_add(1, Ordering::Release);

        info!(
            session = %self.session_key,
            overflow_content_len = msg.content.len(),
            history_len = pre_primary_history.len(),
            active_overflow = current + 1,
            "speculative: spawning full agent task for overflow with own chat bubble"
        );

        // Clone everything needed for the spawned task
        let agent = Arc::clone(&self.agent);
        let session_handle = Arc::clone(&self.session_handle);
        let overflow_counter = Arc::clone(&self.active_overflow_tasks);
        let out_tx = self.out_tx.clone();
        let channel = self.channel.clone();
        let chat_id = self.chat_id.clone();
        let session_key = self.session_key.clone();
        let content = msg.content.clone();
        let overflow_reply_to = msg.message_id.clone();
        let session_timeout = self.session_timeout;
        let status_indicator = self.status_indicator.clone();
        let user_status_config = self.user_status_config.clone();
        let history = pre_primary_history.to_vec();

        tokio::spawn(async move {
            // Save user message to history first
            let user_msg = Message {
                role: MessageRole::User,
                content: content.clone(),
                media: vec![],
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                timestamp: chrono::Utc::now(),
            };
            {
                let mut handle = session_handle.lock().await;
                let _ = handle.add_message(user_msg).await;
            }

            let history: Vec<Message> = history;
            let tracker = Arc::new(TokenTracker::new());

            // ── Per-overflow status indicator (own "✦ Thinking..." message) ──
            let status_handle = status_indicator.as_ref().map(|si| {
                si.start(
                    chat_id.clone(),
                    &content,
                    Arc::clone(&tracker),
                    None,
                    &user_status_config,
                )
            });

            // ── Per-overflow stream reporter (own chat bubble) ──────────────
            let (stream_tx, stream_rx) = tokio::sync::mpsc::unbounded_channel();
            let overflow_reporter: Arc<dyn crew_agent::ProgressReporter> = Arc::new(
                crate::stream_reporter::ChannelStreamReporter::new(stream_tx),
            );

            // Spawn stream forwarder — edits its OWN message, not the primary's
            let stream_forwarder = if let Some(ref si) = status_indicator {
                let fwd_channel = Arc::clone(si.channel());
                let cancel_status = status_handle.as_ref().map(|h| Arc::clone(&h.cancelled));
                let status_msg_id = status_handle.as_ref().map(|h| Arc::clone(&h.status_msg_id));
                Some(tokio::spawn(crate::stream_reporter::run_stream_forwarder(
                    stream_rx,
                    fwd_channel,
                    chat_id.clone(),
                    cancel_status,
                    status_msg_id,
                )))
            } else {
                drop(stream_rx);
                None
            };

            // ── Run agent with task-local reporter override ─────────────────
            let reporter_for_scope = overflow_reporter.clone();
            let result = crew_agent::TASK_REPORTER
                .scope(reporter_for_scope, async {
                    tokio::time::timeout(
                        session_timeout,
                        agent.process_message_tracked(&content, &history, vec![], &tracker),
                    )
                    .await
                })
                .await;

            // Drop the reporter so the stream forwarder sees channel close
            drop(overflow_reporter);

            // Wait for stream forwarder to finish flushing
            let stream_result = if let Some(handle) = stream_forwarder {
                match handle.await {
                    Ok(sr) => Some(sr),
                    Err(_) => None,
                }
            } else {
                None
            };

            // Stop status indicator (deletes the "✦ Thinking..." message)
            if let Some(handle) = status_handle {
                handle.stop().await;
            }

            match result {
                Ok(Ok(conv_response)) => {
                    // Save ONLY the final assistant reply to session history.
                    // Intermediate tool_call/tool_result messages are NOT saved
                    // to avoid tool_call ID collisions when multiple overflow
                    // tasks run concurrently (e.g. two deep_search_0 IDs).
                    {
                        let mut handle = session_handle.lock().await;
                        let final_reply = Message {
                            role: MessageRole::Assistant,
                            content: conv_response.content.clone(),
                            media: vec![],
                            tool_calls: None,
                            tool_call_id: None,
                            reasoning_content: None,
                            timestamp: chrono::Utc::now(),
                        };
                        let _ = handle.add_message(final_reply).await;
                    }

                    let reply = strip_think_tags(&conv_response.content);
                    // If streaming already delivered the reply (edited into
                    // the stream message), skip the duplicate out_tx send.
                    let already_streamed = stream_result
                        .as_ref()
                        .map_or(false, |sr| sr.message_id.is_some());

                    if !reply.trim().is_empty() && !already_streamed {
                        let _ = out_tx
                            .send(OutboundMessage {
                                channel,
                                chat_id,
                                content: reply,
                                reply_to: overflow_reply_to,
                                media: vec![],
                                metadata: serde_json::json!({}),
                            })
                            .await;
                    }
                }
                Ok(Err(e)) => {
                    tracing::error!(session = %session_key, error = %e, "overflow agent task failed");
                    let _ = out_tx
                        .send(OutboundMessage {
                            channel,
                            chat_id,
                            content: format!("Error: {e}"),
                            reply_to: overflow_reply_to,
                            media: vec![],
                            metadata: serde_json::json!({}),
                        })
                        .await;
                }
                Err(_) => {
                    let _ = out_tx
                        .send(OutboundMessage {
                            channel,
                            chat_id,
                            content: "Processing timed out.".to_string(),
                            reply_to: overflow_reply_to,
                            media: vec![],
                            metadata: serde_json::json!({}),
                        })
                        .await;
                }
            }
            // Decrement active overflow counter
            overflow_counter.fetch_sub(1, Ordering::Release);
        });
    }

    async fn process_inbound(&mut self, inbound: InboundMessage, image_media: Vec<String>) {
        // Capture the platform message ID for reply threading
        let inbound_message_id = inbound.message_id.clone();

        // Acquire concurrency permit
        let _permit = match self.semaphore.acquire().await {
            Ok(p) => p,
            Err(_) => return, // semaphore closed
        };

        // Get conversation history
        let max_history = self.max_history.load(Ordering::Acquire);
        let history: Vec<Message> = {
            let mut handle = self.session_handle.lock().await;
            let session = handle.get_or_create();
            session.get_history(max_history).to_vec()
        };

        // Token tracker for status indicator
        let token_tracker = Arc::new(TokenTracker::new());

        // Start status indicator
        let status_handle = self.status_indicator.as_ref().map(|si| {
            let voice_transcript = inbound
                .metadata
                .get("voice_transcript")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            si.start(
                self.chat_id.clone(),
                &inbound.content,
                Arc::clone(&token_tracker),
                voice_transcript,
                &self.user_status_config,
            )
        });

        // Set up progressive streaming reporter if we have a channel
        let (stream_tx, stream_rx) = tokio::sync::mpsc::unbounded_channel();
        let reporter = Arc::new(crate::stream_reporter::ChannelStreamReporter::new(
            stream_tx.clone(),
        ));
        self.agent.set_reporter(reporter);

        // Wire adaptive router status callback for failover notifications
        if let Some(ref router) = self.adaptive_router {
            let status_tx = stream_tx.clone();
            router.set_status_callback(Some(Arc::new(move |message: String| {
                let _ = status_tx
                    .send(crate::stream_reporter::StreamProgressEvent::LlmStatus { message });
            })));
        }

        // Set provider layer on the status composer
        if let Some(ref handle) = status_handle {
            handle.set_provider(self.agent.provider_name(), self.agent.model_id());
        }

        // Spawn stream forwarder task — edits a channel message as text arrives.
        // Only for channels that support message editing (Discord, Telegram, Feishu).
        // Channels without edit support (WeCom bot, Slack, etc.) skip streaming
        // to avoid sending duplicate messages.
        let stream_forwarder = if let Some(ref si) = self.status_indicator {
            let channel = Arc::clone(si.channel());
            if channel.supports_edit() {
                let cancel_status = status_handle.as_ref().map(|h| Arc::clone(&h.cancelled));
                let status_msg_id = status_handle.as_ref().map(|h| Arc::clone(&h.status_msg_id));
                Some(tokio::spawn(crate::stream_reporter::run_stream_forwarder(
                    stream_rx,
                    channel,
                    self.chat_id.clone(),
                    cancel_status,
                    status_msg_id,
                )))
            } else {
                drop(stream_rx);
                None
            }
        } else {
            // No channel available — drop the receiver so events are discarded
            drop(stream_rx);
            None
        };

        // Process through agent (potentially long LLM call)
        let llm_start = Instant::now();
        let result = tokio::time::timeout(
            self.session_timeout,
            self.agent.process_message_tracked(
                &inbound.content,
                &history,
                image_media,
                &token_tracker,
            ),
        )
        .await;
        let llm_latency = llm_start.elapsed();

        // Feed latency to responsiveness observer
        self.responsiveness.record(llm_latency);
        if self.responsiveness.should_activate() {
            warn!(
                session = %self.session_key,
                baseline_ms = ?self.responsiveness.baseline().map(|b| b.as_millis()),
                latency_ms = llm_latency.as_millis(),
                consecutive_slow = self.responsiveness.consecutive_slow_count(),
                "sustained latency degradation detected, activating auto-protection"
            );
            self.responsiveness.set_active(true);
            // Escalate: hedge routing (race providers) + speculative queue (unblock for new messages)
            self.queue_mode = QueueMode::Speculative;
            if let Some(ref router) = self.adaptive_router {
                router.set_mode(AdaptiveMode::Hedge);
                let _ = self.out_tx.send(OutboundMessage {
                    channel: self.channel.clone(),
                    chat_id: self.chat_id.clone(),
                    content: "⚡ Detected slow responses. Enabling hedge racing + speculative queue — you won't be blocked.".to_string(),
                    reply_to: None,
                    media: vec![],
                    metadata: serde_json::json!({}),
                }).await;
            }
        } else if self.responsiveness.should_deactivate() {
            info!(session = %self.session_key, "provider recovered, reverting to normal mode");
            self.responsiveness.set_active(false);
            self.queue_mode = QueueMode::Followup;
            if let Some(ref router) = self.adaptive_router {
                router.set_mode(AdaptiveMode::Off);
            }
        }

        // Reset reporter to silent (drop the stream sender → forwarder will finish)
        self.agent
            .set_reporter(Arc::new(crew_agent::SilentReporter));

        // Clear adaptive router status callback
        if let Some(ref router) = self.adaptive_router {
            router.set_status_callback(None);
        }

        // Wait for stream forwarder to complete and get its result
        let stream_result = if let Some(handle) = stream_forwarder {
            (handle.await).ok()
        } else {
            None
        };

        // Stop status indicator (if stream forwarder didn't already cancel it)
        if let Some(handle) = status_handle {
            handle.stop().await;
        }

        match result {
            Ok(Ok(conv_response)) => {
                // Save all messages from the agent (user msg, tool calls, tool
                // results, assistant replies) so the full context is preserved
                // for subsequent calls.
                {
                    let mut handle = self.session_handle.lock().await;
                    // Auto-generate summary from first user message
                    {
                        let session = handle.get_or_create();
                        if session.summary.is_none() && !inbound.content.trim().is_empty() {
                            let summary: String = inbound.content.chars().take(100).collect();
                            session.summary = Some(summary);
                        }
                    }

                    for msg in &conv_response.messages {
                        if let Err(e) = handle.add_message(msg.clone()).await {
                            warn!(session = %self.session_key, role = ?msg.role, error = %e, "failed to persist message");
                        }
                    }

                    // Compact if needed
                    if let Err(e) = crate::compaction::maybe_compact_handle(
                        &mut handle,
                        &*self.llm_for_compaction,
                    )
                    .await
                    {
                        warn!("session compaction failed: {e}");
                    }
                }

                // Send reply — always goes to this actor's chat (no race!)
                let content = strip_think_tags(&conv_response.content);

                let is_cron = inbound.channel == "system" && inbound.sender_id == "cron";
                let is_silent = content.trim().is_empty()
                    || content.contains("[SILENT]")
                    || content.contains("[NO_CHANGE]");

                if !(is_cron && is_silent) {
                    let display_content = if content.trim().is_empty() && !is_cron {
                        tracing::warn!(session = %self.session_key, "LLM returned empty content, sending fallback");
                        "(The model returned an empty response. Please try again.)".to_string()
                    } else {
                        content
                            .trim_start()
                            .strip_prefix("[SILENT]")
                            .or_else(|| content.trim_start().strip_prefix("[NO_CHANGE]"))
                            .unwrap_or(&content)
                            .to_string()
                    };

                    // If stream forwarder already sent a message, do a final edit
                    // with the clean content instead of sending a new message.
                    let streamed = if let Some(ref sr) = stream_result {
                        if let Some(ref mid) = sr.message_id {
                            if let Some(ref si) = self.status_indicator {
                                // Strip tool status lines from final content
                                let _ = si
                                    .channel()
                                    .edit_message(&self.chat_id, mid, &display_content)
                                    .await;
                            }
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };

                    if !streamed {
                        let _ = self
                            .out_tx
                            .send(OutboundMessage {
                                channel: self.channel.clone(),
                                chat_id: self.chat_id.clone(),
                                content: display_content,
                                reply_to: inbound_message_id.clone(),
                                media: vec![],
                                metadata: serde_json::json!({}),
                            })
                            .await;
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::error!(session = %self.session_key, error = %e, "agent processing failed");
                let _ = self
                    .out_tx
                    .send(OutboundMessage {
                        channel: self.channel.clone(),
                        chat_id: self.chat_id.clone(),
                        content: format!("Error: {e}"),
                        reply_to: inbound_message_id.clone(),
                        media: vec![],
                        metadata: serde_json::json!({}),
                    })
                    .await;
            }
            Err(_) => {
                tracing::error!(session = %self.session_key, "session processing timed out");
                let _ = self
                    .out_tx
                    .send(OutboundMessage {
                        channel: self.channel.clone(),
                        chat_id: self.chat_id.clone(),
                        content: "Processing timed out. Please try again.".to_string(),
                        reply_to: inbound_message_id.clone(),
                        media: vec![],
                        metadata: serde_json::json!({}),
                    })
                    .await;
            }
        }

        // Send completion marker so the API channel can close the SSE stream.
        if self.channel == "api" {
            let _ = self
                .out_tx
                .send(OutboundMessage {
                    channel: self.channel.clone(),
                    chat_id: self.chat_id.clone(),
                    content: String::new(),
                    reply_to: None,
                    media: vec![],
                    metadata: serde_json::json!({"_completion": true}),
                })
                .await;
        }
    }
}

/// Strip `<think>...</think>` blocks that some models embed inline.
fn strip_think_tags(s: &str) -> String {
    let mut result = s.to_string();
    while let Some(start) = result.find("<think>") {
        if let Some(end) = result[start..].find("</think>") {
            result.replace_range(start..start + end + "</think>".len(), "");
        } else {
            result.truncate(start);
            break;
        }
    }
    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use crew_llm::{AdaptiveConfig, ChatConfig, ChatResponse, StopReason, TokenUsage, ToolSpec};
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn test_strip_think_tags() {
        assert_eq!(strip_think_tags("hello"), "hello");
        assert_eq!(strip_think_tags("<think>hmm</think>hello"), "hello");
        assert_eq!(
            strip_think_tags("before<think>hmm</think>after"),
            "beforeafter"
        );
        assert_eq!(strip_think_tags("<think>unclosed"), "");
    }

    // ── Mock providers for speculative overflow tests ────────────────────

    /// Mock LLM provider with configurable delay per call.
    /// Returns scripted responses in FIFO order.
    struct DelayedMockProvider {
        responses: std::sync::Mutex<Vec<(Duration, ChatResponse)>>,
        call_count: AtomicUsize,
        name: String,
    }

    impl DelayedMockProvider {
        fn new(name: &str, responses: Vec<(Duration, ChatResponse)>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
                call_count: AtomicUsize::new(0),
                name: name.to_string(),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for DelayedMockProvider {
        async fn chat(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _config: &ChatConfig,
        ) -> eyre::Result<ChatResponse> {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            let (delay, response) = {
                let mut responses = self.responses.lock().unwrap();
                if responses.is_empty() {
                    return Ok(ChatResponse {
                        content: Some("(no more scripted responses)".into()),
                        reasoning_content: None,
                        tool_calls: vec![],
                        stop_reason: StopReason::EndTurn,
                        usage: TokenUsage::default(),
                    });
                }
                responses.remove(0)
            };
            tokio::time::sleep(delay).await;
            Ok(response)
        }

        fn context_window(&self) -> u32 {
            128_000
        }

        fn model_id(&self) -> &str {
            &self.name
        }

        fn provider_name(&self) -> &str {
            &self.name
        }
    }

    fn make_response(text: &str) -> ChatResponse {
        ChatResponse {
            content: Some(text.to_string()),
            reasoning_content: None,
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage {
                input_tokens: 50,
                output_tokens: 10,
                ..Default::default()
            },
        }
    }

    fn make_inbound(content: &str) -> ActorMessage {
        ActorMessage::Inbound {
            message: InboundMessage {
                channel: "cli".to_string(),
                chat_id: "test".to_string(),
                sender_id: "user".to_string(),
                content: content.to_string(),
                timestamp: chrono::Utc::now(),
                media: vec![],
                metadata: serde_json::json!({}),
                message_id: None,
            },
            image_media: vec![],
        }
    }

    /// Build a SessionActor with configurable queue mode and optional adaptive router.
    ///
    /// Generic setup used by queue mode, auto-escalation, and other tests.
    /// `adaptive_router` controls whether speculative overflow is available.
    /// `pre_seed_baseline`: if true, pre-seeds 5×500ms to establish responsiveness baseline.
    async fn setup_actor_with_mode(
        agent_provider: Arc<dyn LlmProvider>,
        queue_mode: QueueMode,
        adaptive_router: Option<Arc<AdaptiveRouter>>,
        pre_seed_baseline: bool,
        dir: &tempfile::TempDir,
    ) -> (
        mpsc::Sender<ActorMessage>,
        mpsc::Receiver<OutboundMessage>,
        JoinHandle<()>,
        Arc<Mutex<SessionManager>>,
    ) {
        let session_mgr = Arc::new(Mutex::new(
            SessionManager::open(&dir.path().join("sessions")).unwrap(),
        ));
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let tools = crew_agent::ToolRegistry::with_builtins(dir.path());

        let agent = Agent::new(AgentId::new("test-mode"), agent_provider, tools, memory)
            .with_config(AgentConfig {
                save_episodes: false,
                max_iterations: 1,
                ..Default::default()
            });

        let (inbox_tx, inbox_rx) = mpsc::channel(32);
        let (out_tx, out_rx) = mpsc::channel(64);

        let mut responsiveness = ResponsivenessObserver::new();
        if pre_seed_baseline {
            for _ in 0..5 {
                responsiveness.record(Duration::from_millis(500));
            }
        }

        let actor = SessionActor {
            session_key: SessionKey::new("cli", "test"),
            channel: "cli".to_string(),
            chat_id: "test".to_string(),
            inbox: inbox_rx,
            agent: Arc::new(agent),
            session_handle: Arc::new(Mutex::new(SessionHandle::open(
                dir.path(),
                &SessionKey::new("cli", "test"),
            ))),
            llm_for_compaction: Arc::new(DelayedMockProvider::new(
                "compaction",
                vec![(Duration::ZERO, make_response("compacted"))],
            )),
            out_tx,
            status_indicator: None,
            user_status_config: UserStatusConfig::default(),
            data_dir: std::path::PathBuf::from("/tmp"),
            max_history: Arc::new(std::sync::atomic::AtomicUsize::new(50)),
            idle_timeout: Duration::from_secs(60),
            session_timeout: Duration::from_secs(120),
            semaphore: Arc::new(Semaphore::new(10)),
            global_shutdown: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
            queue_mode,
            responsiveness,
            adaptive_router,
            memory_store: None,
            active_overflow_tasks: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        };

        let handle = tokio::spawn(actor.run());
        (inbox_tx, out_rx, handle, session_mgr)
    }

    /// Build a minimal SessionActor with speculative mode + adaptive router.
    ///
    /// `agent_provider` is used by the Agent for primary calls.
    /// `router_providers` are used by the AdaptiveRouter for overflow calls.
    /// These MUST be separate instances (separate response queues).
    async fn setup_speculative_actor(
        agent_provider: Arc<dyn LlmProvider>,
        router_providers: Vec<Arc<dyn LlmProvider>>,
        dir: &tempfile::TempDir,
    ) -> (
        mpsc::Sender<ActorMessage>,
        mpsc::Receiver<OutboundMessage>,
        JoinHandle<()>,
        Arc<Mutex<SessionManager>>,
    ) {
        let session_mgr = Arc::new(Mutex::new(
            SessionManager::open(&dir.path().join("sessions")).unwrap(),
        ));
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let tools = crew_agent::ToolRegistry::with_builtins(dir.path());

        let agent = Agent::new(AgentId::new("test-spec"), agent_provider, tools, memory)
            .with_config(AgentConfig {
                save_episodes: false,
                max_iterations: 1,
                ..Default::default()
            });

        // AdaptiveRouter with separate providers for overflow (serve_overflow only)
        let router = Arc::new(
            AdaptiveRouter::new(router_providers, AdaptiveConfig::default())
                .with_adaptive_config(AdaptiveMode::Hedge, false),
        );

        let (inbox_tx, inbox_rx) = mpsc::channel(32);
        let (out_tx, out_rx) = mpsc::channel(64);

        // Pre-seed responsiveness baseline so patience = 10s (not 30s default)
        let mut responsiveness = ResponsivenessObserver::new();
        for _ in 0..5 {
            responsiveness.record(Duration::from_millis(500));
        }
        // baseline = 500ms → patience = max(1000ms, 10s) = 10s
        // But we want lower patience for fast tests. We'll use 2s responses
        // to establish baseline=2s → patience=max(4s, 10s)=10s.
        // For the test, the slow call takes 15s, so 15s > 10s triggers overflow.

        let actor = SessionActor {
            session_key: SessionKey::new("cli", "test"),
            channel: "cli".to_string(),
            chat_id: "test".to_string(),
            inbox: inbox_rx,
            agent: Arc::new(agent),
            session_handle: Arc::new(Mutex::new(SessionHandle::open(
                dir.path(),
                &SessionKey::new("cli", "test"),
            ))),
            llm_for_compaction: Arc::new(DelayedMockProvider::new(
                "compaction",
                vec![(Duration::ZERO, make_response("compacted"))],
            )),
            out_tx,
            status_indicator: None,
            user_status_config: UserStatusConfig::default(),
            data_dir: std::path::PathBuf::from("/tmp"),
            max_history: Arc::new(std::sync::atomic::AtomicUsize::new(50)),
            idle_timeout: Duration::from_secs(60),
            session_timeout: Duration::from_secs(120),
            semaphore: Arc::new(Semaphore::new(10)),
            global_shutdown: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
            queue_mode: QueueMode::Speculative,
            responsiveness,
            adaptive_router: Some(router),
            memory_store: None,
            active_overflow_tasks: Arc::new(std::sync::atomic::AtomicU32::new(0)),
        };

        let handle = tokio::spawn(actor.run());
        (inbox_tx, out_rx, handle, session_mgr)
    }

    /// Core speculative overflow test:
    /// - Send a message that triggers a slow (3s) agent call
    /// - After 1s, send an overflow message
    /// - The overflow should be served via serve_overflow while the slow call continues
    /// - Both responses should arrive
    #[tokio::test]
    async fn test_speculative_overflow_concurrent() {
        let dir = tempfile::TempDir::new().unwrap();

        // Agent provider: 5 fast warmups + 1 slow (12s) primary call
        // + 1 fast overflow response (serve_overflow now uses the agent)
        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![
                (Duration::from_millis(200), make_response("warmup1")),
                (Duration::from_millis(200), make_response("warmup2")),
                (Duration::from_millis(200), make_response("warmup3")),
                (Duration::from_millis(200), make_response("warmup4")),
                (Duration::from_millis(200), make_response("warmup5")),
                // Slow call that triggers overflow (12s > 10s patience)
                (
                    Duration::from_secs(12),
                    make_response("slow primary answer"),
                ),
                // Overflow agent task (runs concurrently with slow primary)
                (
                    Duration::from_millis(500),
                    make_response("overflow answer: 1961"),
                ),
                (Duration::from_millis(200), make_response("post-overflow")),
            ],
        ));

        // Router providers (separate instances, used ONLY by serve_overflow)
        let router_a: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new(
            "router-a",
            vec![
                (
                    Duration::from_millis(500),
                    make_response("router-a overflow"),
                ),
                (Duration::from_millis(500), make_response("router-a extra")),
            ],
        ));
        let router_b: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new(
            "router-b",
            vec![
                (
                    Duration::from_millis(100),
                    make_response("overflow answer: 1961"),
                ),
                (Duration::from_millis(100), make_response("router-b extra")),
            ],
        ));

        let (tx, mut rx, handle, session_mgr) =
            setup_speculative_actor(agent_llm, vec![router_a, router_b], &dir).await;

        // ── Phase 1: Warm-up (5 fast messages to establish baseline) ──
        for i in 0..5 {
            tx.send(make_inbound(&format!("warmup {i}"))).await.unwrap();
            // Wait for response
            let resp = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("warmup response timeout")
                .expect("channel closed");
            assert!(!resp.content.is_empty(), "warmup {i} got empty response");
        }

        // ── Phase 2: Send slow request, then overflow ──
        tx.send(make_inbound("Do a complex multi-step analysis"))
            .await
            .unwrap();

        // Wait 11s for patience (10s) to be exceeded, then send overflow
        tokio::time::sleep(Duration::from_secs(11)).await;

        tx.send(make_inbound("What is 37 * 53?")).await.unwrap();

        // ── Phase 3: Collect all responses ──
        // We expect 2 responses: overflow answer + slow primary answer (in some order)
        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        while responses.len() < 2 {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(msg)) => responses.push(msg.content),
                Ok(None) => break,
                Err(_) => break, // timeout
            }
        }

        assert!(
            responses.len() >= 2,
            "expected at least 2 responses (overflow + primary), got {}: {:?}",
            responses.len(),
            responses
        );

        // One should be the overflow answer, one the primary (with ⬆️ marker)
        let has_overflow = responses
            .iter()
            .any(|r| r.contains("1961") || r.contains("overflow"));
        let has_primary = responses
            .iter()
            .any(|r| r.contains("slow primary") || r.contains("primary"));

        assert!(
            has_overflow,
            "overflow response not found in: {:?}",
            responses
        );
        assert!(
            has_primary,
            "primary response not found in: {:?}",
            responses
        );

        // ── Phase 4: Verify history is sorted by timestamp ──
        {
            // Reload from disk (actor writes via its own SessionHandle to per-user dir)
            let handle = SessionHandle::open(dir.path(), &SessionKey::new("cli", "test"));
            let session = handle.session();
            let messages = &session.messages;
            assert!(
                messages.len() >= 4,
                "expected at least 4 messages in history (warmups + primary + overflow), got {}",
                messages.len()
            );

            // Verify timestamps are sorted
            for window in messages.windows(2) {
                assert!(
                    window[0].timestamp <= window[1].timestamp,
                    "history not sorted: {:?} > {:?} (contents: '{}' vs '{}')",
                    window[0].timestamp,
                    window[1].timestamp,
                    &window[0].content[..window[0].content.len().min(50)],
                    &window[1].content[..window[1].content.len().min(50)],
                );
            }
        }

        // Clean shutdown
        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// Test that messages within patience threshold are NOT served as overflow.
    #[tokio::test]
    async fn test_speculative_within_patience_serves_both() {
        let dir = tempfile::TempDir::new().unwrap();

        // Agent: 5 warmups + primary (5s) + overflow (fast)
        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![
                (Duration::from_millis(200), make_response("w1")),
                (Duration::from_millis(200), make_response("w2")),
                (Duration::from_millis(200), make_response("w3")),
                (Duration::from_millis(200), make_response("w4")),
                (Duration::from_millis(200), make_response("w5")),
                (Duration::from_secs(5), make_response("primary done")),
                (Duration::from_millis(100), make_response("overflow done")),
            ],
        ));

        let router_a: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new(
            "router-a",
            vec![(Duration::from_millis(100), make_response("unused"))],
        ));
        let router_b: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new(
            "router-b",
            vec![(Duration::from_millis(100), make_response("unused"))],
        ));

        let (tx, mut rx, handle, _session_mgr) =
            setup_speculative_actor(agent_llm, vec![router_a, router_b], &dir).await;

        // Warm-up
        for i in 0..5 {
            tx.send(make_inbound(&format!("warmup {i}"))).await.unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        }

        // Send primary (5s)
        tx.send(make_inbound("medium task")).await.unwrap();

        // Send overflow at 2s (within 10s patience) — should still be served
        tokio::time::sleep(Duration::from_secs(2)).await;
        tx.send(make_inbound("quick question")).await.unwrap();

        // Collect responses — should get 2 (both overflow and primary)
        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(msg)) => responses.push(msg.content),
                _ => break,
            }
        }

        assert_eq!(
            responses.len(),
            2,
            "expected 2 responses (overflow + primary), got {}: {:?}",
            responses.len(),
            responses
        );
        // Overflow finishes first (fast), primary finishes second (5s)
        assert!(
            responses.iter().any(|r| r.contains("overflow done")),
            "expected overflow response, got: {:?}",
            responses
        );
        assert!(
            responses.iter().any(|r| r.contains("primary done")),
            "expected primary response, got: {:?}",
            responses
        );

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// Test that background results are handled during speculative select loop.
    #[tokio::test]
    async fn test_speculative_handles_background_result() {
        let dir = tempfile::TempDir::new().unwrap();

        // Agent: 5 warmups + 8s primary
        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![
                (Duration::from_millis(200), make_response("w1")),
                (Duration::from_millis(200), make_response("w2")),
                (Duration::from_millis(200), make_response("w3")),
                (Duration::from_millis(200), make_response("w4")),
                (Duration::from_millis(200), make_response("w5")),
                (Duration::from_secs(8), make_response("primary done")),
            ],
        ));

        // Router providers (not used in this test — no overflow messages sent)
        let router_a: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new("router-a", vec![]));
        let router_b: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new("router-b", vec![]));

        let (tx, mut rx, handle, session_mgr) =
            setup_speculative_actor(agent_llm, vec![router_a, router_b], &dir).await;

        // Warm-up
        for i in 0..5 {
            tx.send(make_inbound(&format!("warmup {i}"))).await.unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;
        }

        // Send primary (8s)
        tx.send(make_inbound("long task")).await.unwrap();

        // Inject background result at 2s (during the speculative select loop)
        tokio::time::sleep(Duration::from_secs(2)).await;
        tx.send(ActorMessage::BackgroundResult {
            task_label: "research".to_string(),
            content: "Background research completed with 5 findings.".to_string(),
        })
        .await
        .unwrap();

        // Collect responses — expect: background notification + primary
        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
        while responses.len() < 2 {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(msg)) => responses.push(msg.content),
                _ => break,
            }
        }

        let has_bg_notification = responses
            .iter()
            .any(|r| r.contains("research") && r.contains("completed"));
        let has_primary = responses.iter().any(|r| r.contains("primary done"));

        assert!(
            has_bg_notification,
            "background result notification not found in: {:?}",
            responses
        );
        assert!(
            has_primary,
            "primary response not found in: {:?}",
            responses
        );

        // Verify background result is in session history
        {
            // Reload from disk (actor writes via its own SessionHandle to per-user dir)
            let handle = SessionHandle::open(dir.path(), &SessionKey::new("cli", "test"));
            let session = handle.session();
            let has_bg_msg = session
                .messages
                .iter()
                .any(|m| m.content.contains("Background task") && m.content.contains("research"));
            assert!(has_bg_msg, "background result not found in session history");
        }

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    // ── Queue mode tests ─────────────────────────────────────────────────

    /// Collect mode batches queued messages into one combined prompt.
    #[tokio::test]
    async fn test_queue_mode_collect_batches() {
        let dir = tempfile::TempDir::new().unwrap();

        // Agent: 1st call slow (2s), 2nd call fast
        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![
                (Duration::from_secs(2), make_response("first reply")),
                (Duration::from_millis(200), make_response("batched reply")),
            ],
        ));

        let (tx, mut rx, handle, session_mgr) =
            setup_actor_with_mode(agent_llm, QueueMode::Collect, None, false, &dir).await;

        // Send first message → starts 2s processing
        tx.send(make_inbound("first message")).await.unwrap();

        // Wait for actor to start processing, then queue two more
        tokio::time::sleep(Duration::from_millis(200)).await;
        tx.send(make_inbound("second message")).await.unwrap();
        tx.send(make_inbound("third message")).await.unwrap();

        // Collect responses (expect 2: first reply + batched reply)
        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while responses.len() < 2 {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(msg)) => responses.push(msg.content),
                _ => break,
            }
        }

        assert_eq!(
            responses.len(),
            2,
            "expected 2 responses (first + batched), got {}: {:?}",
            responses.len(),
            responses
        );

        // Verify session history: second user message should contain batched content
        {
            // Reload from disk (actor writes via its own SessionHandle to per-user dir)
            let handle = SessionHandle::open(dir.path(), &SessionKey::new("cli", "test"));
            let session = handle.session();
            let user_messages: Vec<&str> = session
                .messages
                .iter()
                .filter(|m| m.role == MessageRole::User)
                .map(|m| m.content.as_str())
                .collect();
            // First user msg: "first message"
            assert!(
                user_messages.iter().any(|m| *m == "first message"),
                "first message not found: {:?}",
                user_messages
            );
            // Second user msg: combined "second message\n---\nQueued #1: third message"
            assert!(
                user_messages
                    .iter()
                    .any(|m| m.contains("second message") && m.contains("third message")),
                "batched message not found: {:?}",
                user_messages
            );
        }

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// Steer mode keeps only the newest queued message, discards older ones.
    #[tokio::test]
    async fn test_queue_mode_steer_keeps_newest() {
        let dir = tempfile::TempDir::new().unwrap();

        // Agent: 1st call slow (2s), 2nd call fast
        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![
                (Duration::from_secs(2), make_response("first reply")),
                (Duration::from_millis(200), make_response("steered reply")),
            ],
        ));

        let (tx, mut rx, handle, session_mgr) =
            setup_actor_with_mode(agent_llm, QueueMode::Steer, None, false, &dir).await;

        // Send first message → starts 2s processing
        tx.send(make_inbound("first message")).await.unwrap();

        // Wait then queue two more — steer should discard "second" and keep "third"
        tokio::time::sleep(Duration::from_millis(200)).await;
        tx.send(make_inbound("second message (discarded)"))
            .await
            .unwrap();
        tx.send(make_inbound("third message (newest)"))
            .await
            .unwrap();

        // Collect responses (expect 2: first + steered)
        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while responses.len() < 2 {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(msg)) => responses.push(msg.content),
                _ => break,
            }
        }

        assert_eq!(
            responses.len(),
            2,
            "expected 2 responses, got {}: {:?}",
            responses.len(),
            responses
        );

        // Verify session history: "second message" should NOT appear as a user message
        {
            // Reload from disk (actor writes via its own SessionHandle to per-user dir)
            let handle = SessionHandle::open(dir.path(), &SessionKey::new("cli", "test"));
            let session = handle.session();
            let user_messages: Vec<&str> = session
                .messages
                .iter()
                .filter(|m| m.role == MessageRole::User)
                .map(|m| m.content.as_str())
                .collect();
            assert!(
                user_messages.iter().any(|m| m.contains("third message")),
                "steered (newest) message not found: {:?}",
                user_messages
            );
            assert!(
                !user_messages.iter().any(|m| m.contains("second message")),
                "discarded message should not be in history: {:?}",
                user_messages
            );
        }

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// Followup mode processes each message individually (no batching).
    #[tokio::test]
    async fn test_queue_mode_followup_sequential() {
        let dir = tempfile::TempDir::new().unwrap();

        // Agent: 3 fast responses
        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![
                (Duration::from_millis(100), make_response("reply-1")),
                (Duration::from_millis(100), make_response("reply-2")),
                (Duration::from_millis(100), make_response("reply-3")),
            ],
        ));

        let (tx, mut rx, handle, session_mgr) =
            setup_actor_with_mode(agent_llm, QueueMode::Followup, None, false, &dir).await;

        // Send 3 messages
        tx.send(make_inbound("msg-a")).await.unwrap();
        tx.send(make_inbound("msg-b")).await.unwrap();
        tx.send(make_inbound("msg-c")).await.unwrap();

        // Collect all 3 responses
        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while responses.len() < 3 {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(msg)) => responses.push(msg.content),
                _ => break,
            }
        }

        assert_eq!(
            responses.len(),
            3,
            "expected 3 sequential responses, got {}: {:?}",
            responses.len(),
            responses
        );

        // All 3 user messages should be in history individually
        {
            // Reload from disk (actor writes via its own SessionHandle to per-user dir)
            let handle = SessionHandle::open(dir.path(), &SessionKey::new("cli", "test"));
            let session = handle.session();
            let user_messages: Vec<&str> = session
                .messages
                .iter()
                .filter(|m| m.role == MessageRole::User)
                .map(|m| m.content.as_str())
                .collect();
            assert!(user_messages.contains(&"msg-a"));
            assert!(user_messages.contains(&"msg-b"));
            assert!(user_messages.contains(&"msg-c"));
        }

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    // ── Auto-escalation tests ────────────────────────────────────────────

    /// Sustained latency degradation triggers auto-escalation to Hedge + Speculative.
    #[tokio::test]
    async fn test_auto_escalation_on_degradation() {
        let dir = tempfile::TempDir::new().unwrap();

        // Agent: 5×100ms warmups + 3×400ms slow (triggers activation at 3× baseline)
        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![
                (Duration::from_millis(100), make_response("warm1")),
                (Duration::from_millis(100), make_response("warm2")),
                (Duration::from_millis(100), make_response("warm3")),
                (Duration::from_millis(100), make_response("warm4")),
                (Duration::from_millis(100), make_response("warm5")),
                (Duration::from_millis(400), make_response("slow1")),
                (Duration::from_millis(400), make_response("slow2")),
                (Duration::from_millis(400), make_response("slow3")),
            ],
        ));

        // Router needed for set_mode call during escalation
        let router_a: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new("r-a", vec![]));
        let router_b: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new("r-b", vec![]));
        let router = Arc::new(
            AdaptiveRouter::new(vec![router_a, router_b], AdaptiveConfig::default())
                .with_adaptive_config(AdaptiveMode::Off, false),
        );
        assert_eq!(router.mode(), AdaptiveMode::Off);

        let (tx, mut rx, handle, _) = setup_actor_with_mode(
            agent_llm,
            QueueMode::Followup,
            Some(router.clone()),
            false, // Let warmups establish baseline naturally
            &dir,
        )
        .await;

        // Send all 8 messages (5 warmup + 3 slow) and collect ALL responses.
        // The "⚡" notification is sent BEFORE the reply in process_inbound,
        // so it can arrive interleaved with normal responses.
        let mut all_responses = Vec::new();
        for i in 0..8 {
            let label = if i < 5 {
                format!("warmup {i}")
            } else {
                format!("slow {}", i - 5)
            };
            tx.send(make_inbound(&label)).await.unwrap();
            // Collect all available responses (may be 1 or 2 if "⚡" arrived)
            loop {
                match tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
                    Ok(Some(msg)) => {
                        let is_notification = msg.content.contains("⚡");
                        all_responses.push(msg.content);
                        if !is_notification {
                            break; // Got the actual reply, move to next message
                        }
                        // If it was the notification, keep reading for the reply
                    }
                    _ => break,
                }
            }
        }

        let found_escalation = all_responses.iter().any(|r| r.contains("⚡"));
        assert!(
            found_escalation,
            "expected ⚡ escalation notification in responses: {:?}",
            all_responses
        );
        assert_eq!(
            router.mode(),
            AdaptiveMode::Hedge,
            "router should be in Hedge mode after escalation"
        );

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    /// Recovery after auto-escalation restores normal mode (Off + Followup).
    #[tokio::test]
    async fn test_auto_deescalation_on_recovery() {
        let dir = tempfile::TempDir::new().unwrap();

        // Agent: 5×100ms warmups + 3×400ms slow + 1×100ms recovery
        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![
                (Duration::from_millis(100), make_response("w1")),
                (Duration::from_millis(100), make_response("w2")),
                (Duration::from_millis(100), make_response("w3")),
                (Duration::from_millis(100), make_response("w4")),
                (Duration::from_millis(100), make_response("w5")),
                (Duration::from_millis(400), make_response("s1")),
                (Duration::from_millis(400), make_response("s2")),
                (Duration::from_millis(400), make_response("s3")),
                // Recovery: fast response resets consecutive_slow → deactivation
                (Duration::from_millis(100), make_response("recovered")),
            ],
        ));

        let router_a: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new("r-a", vec![]));
        let router_b: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new("r-b", vec![]));
        let router = Arc::new(
            AdaptiveRouter::new(vec![router_a, router_b], AdaptiveConfig::default())
                .with_adaptive_config(AdaptiveMode::Off, false),
        );

        let (tx, mut rx, handle, _) = setup_actor_with_mode(
            agent_llm,
            QueueMode::Followup,
            Some(router.clone()),
            false,
            &dir,
        )
        .await;

        // Warmup + degradation (same as escalation test)
        for i in 0..8 {
            tx.send(make_inbound(&format!("msg {i}"))).await.unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(3), rx.recv()).await;
        }

        // Drain the escalation notification
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(msg)) if msg.content.contains("⚡") => break,
                Ok(Some(_)) => continue,
                _ => break,
            }
        }

        // Verify escalated state
        assert_eq!(router.mode(), AdaptiveMode::Hedge);

        // Send recovery message (fast 100ms → resets consecutive_slow to 0)
        // After escalation, queue_mode changed to Speculative internally.
        // The speculative path also records latency and checks deactivation.
        tx.send(make_inbound("recovery ping")).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await;

        // Give the actor a moment to process the deactivation
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Router should be back to Off mode
        assert_eq!(
            router.mode(),
            AdaptiveMode::Off,
            "router should revert to Off after recovery"
        );

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }
}
