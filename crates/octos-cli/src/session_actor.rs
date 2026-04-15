//! Session actor: per-session tokio task that owns tools and processes messages.
//!
//! Replaces the spawn-per-message model in the gateway, eliminating the
//! `set_context()` race condition where shared tools could route messages
//! to the wrong chat.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex as StdMutex, Weak};
use std::time::{Duration, Instant};

use octos_agent::tools::{
    BackgroundResultKind, BackgroundResultPayload, CheckBackgroundTasksTool, MessageTool,
    SendFileTool, SpawnTool, ToolPolicy, ToolRegistry,
};
use octos_agent::{
    Agent, AgentConfig, HookContext, HookExecutor, TaskSupervisor, TokenTracker,
    TurnAttachmentContext, WorkspacePolicy, read_workspace_policy, workspace_policy_path,
    write_workspace_policy,
};
use octos_bus::{ActiveSessionStore, SessionHandle, SessionManager};
use octos_core::AgentId;
use octos_core::{
    InboundMessage, MAIN_PROFILE_ID, METADATA_SENDER_USER_ID, Message, MessageRole,
    OutboundMessage, SessionKey,
};
use octos_llm::{
    AdaptiveMode, AdaptiveRouter, EmbeddingProvider, LlmProvider, ProviderRouter,
    ResponsivenessObserver,
};
use octos_memory::{EpisodeStore, MemoryStore};
use tokio::sync::{Mutex, RwLock, Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::config::QueueMode;
use crate::cron_tool::CronTool;
use crate::status_layers::{StatusComposer, UserStatusConfig};

/// Parameters for dispatching an inbound message to a session actor.
pub struct DispatchParams<'a> {
    pub message: InboundMessage,
    pub image_media: Vec<String>,
    pub attachment_media: Vec<String>,
    pub attachment_prompt: Option<String>,
    pub session_key: SessionKey,
    pub reply_channel: &'a str,
    pub reply_chat_id: &'a str,
    pub status_indicator: Option<Arc<StatusComposer>>,
    pub profile_id: Option<&'a str>,
    pub system_prompt_override: Option<String>,
    pub sender_user_id: Option<String>,
}

/// Parameters for spawning a new session actor.
struct SpawnParams<'a> {
    session_key: SessionKey,
    channel: &'a str,
    chat_id: &'a str,
    semaphore: Arc<Semaphore>,
    status_indicator: Option<Arc<StatusComposer>>,
    system_prompt_override: Option<String>,
    sender_user_id: Option<String>,
}

/// Parameters for the outbound message forwarder task.
struct ForwarderParams {
    proxy_rx: mpsc::Receiver<OutboundMessage>,
    out_tx: mpsc::Sender<OutboundMessage>,
    session_key: SessionKey,
    channel: String,
    chat_id: String,
    active_sessions: Arc<RwLock<ActiveSessionStore>>,
    pending_messages: PendingMessages,
    sender_user_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ForcedBackgroundWorkflow {
    DeepResearch,
    ResearchPodcast,
}

impl ForcedBackgroundWorkflow {
    fn detect(content: &str) -> Option<Self> {
        let lower = content.to_ascii_lowercase();
        if Self::explicitly_foreground(&lower, content) {
            return None;
        }

        let has_podcast =
            lower.contains("podcast") || content.contains("播客") || content.contains("语音播客");
        let has_research_signal = lower.contains("deep research")
            || lower.contains("research")
            || lower.contains("latest")
            || lower.contains("news")
            || content.contains("研究")
            || content.contains("深入")
            || content.contains("深度")
            || content.contains("最新")
            || content.contains("今日")
            || content.contains("热点")
            || content.contains("新闻")
            || content.contains("搜索")
            || content.contains("资料");

        if has_podcast && has_research_signal {
            return Some(Self::ResearchPodcast);
        }

        let has_deep_research = lower.contains("deep research")
            || content.contains("深度研究")
            || content.contains("深入研究")
            || content.contains("深度调查")
            || content.contains("深度搜索")
            || content.contains("深度调研");
        if has_deep_research {
            return Some(Self::DeepResearch);
        }

        None
    }

    fn explicitly_foreground(lower: &str, original: &str) -> bool {
        lower.contains("wait synchronously")
            || lower.contains("wait for completion")
            || lower.contains("don't use background")
            || lower.contains("do not use background")
            || original.contains("不要后台")
            || original.contains("别后台")
            || original.contains("同步")
            || original.contains("等待完成")
    }

    fn label(self) -> &'static str {
        match self {
            Self::DeepResearch => "Deep research",
            Self::ResearchPodcast => "Research podcast",
        }
    }

    fn ack_message(self) -> &'static str {
        match self {
            Self::DeepResearch => "深度研究已在后台启动。完成后会把最终研究结果发回当前会话。",
            Self::ResearchPodcast => {
                "研究和播客生成已在后台启动。完成后只会发送最终音频结果到当前会话。"
            }
        }
    }

    fn allowed_tools(self) -> Vec<String> {
        match self {
            Self::DeepResearch => vec!["run_pipeline".into()],
            Self::ResearchPodcast => vec![
                "deep_search".into(),
                "news_fetch".into(),
                "write_file".into(),
                "read_file".into(),
                "list_dir".into(),
                "glob".into(),
                "podcast_generate".into(),
            ],
        }
    }

    fn additional_instructions(self) -> &'static str {
        match self {
            Self::DeepResearch => {
                "You are a background research analyst. Use run_pipeline with an inline DOT graph to complete the research in the background. Deliver exactly one final user-facing report. Do not emit intermediate status chatter or send intermediate files."
            }
            Self::ResearchPodcast => {
                "You are a background news podcast producer. Research inside this worker, write a single podcast script in the exact format `[Character - voice, emotion] text`, and then call podcast_generate exactly once after the script is ready. Use the exact clone voices `clone:yangmi` for 杨幂 and `clone:douwentao` for 窦文涛 on every dialogue line. Do not substitute preset voices like alloy, nova, or vivian. Keep the research focused: gather only enough fresh evidence to support the script, stop after roughly 4-6 search passes, and do not keep recursively expanding side topics. Target a substantive but bounded final audio runtime of about 10-15 minutes unless the user explicitly asks for a longer show. That usually means about 18-28 dialogue lines total and no sprawling 30-45 minute scripts. Do not use fm_tts, voice_synthesize, or send_file. Do not deliver intermediate reports or script files. Only the final podcast audio may be delivered to the user."
            }
        }
    }
}

/// Default actor inbox capacity.
const ACTOR_INBOX_SIZE: usize = 32;

/// Default idle timeout before an actor shuts down (30 minutes).
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 1800;

/// Maximum concurrent overflow tasks per session.
const MAX_OVERFLOW_TASKS: u32 = 5;

/// Maximum number of pending messages buffered per inactive session.
const MAX_PENDING_PER_SESSION: usize = 50;

#[derive(Debug, Clone, serde::Serialize)]
struct PersistedSessionMessage {
    seq: usize,
    timestamp: chrono::DateTime<chrono::Utc>,
}

async fn persist_assistant_message(
    session_handle: &Arc<Mutex<SessionHandle>>,
    session_key: &SessionKey,
    content: String,
    media: Vec<String>,
) -> Option<PersistedSessionMessage> {
    let mut assistant_msg = Message::assistant(content);
    assistant_msg.media = media;
    let timestamp = assistant_msg.timestamp;

    let mut handle = session_handle.lock().await;
    match handle.add_message_with_seq(assistant_msg).await {
        Ok(seq) => Some(PersistedSessionMessage { seq, timestamp }),
        Err(error) => {
            warn!(
                session = %session_key,
                error = %error,
                "failed to persist assistant message"
            );
            None
        }
    }
}

fn resolve_builtin_slides_styles_dir(data_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let current_profile_id = data_dir
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string());

    let family_root_profile = current_profile_id
        .as_deref()
        .and_then(|value| value.split("--").next())
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string());

    let octos_home = data_dir
        .ancestors()
        .nth(3)
        .map(std::path::Path::to_path_buf);

    let mut candidates = Vec::new();
    candidates.push(data_dir.join("skills").join("mofa-slides").join("styles"));

    if let Some(ref home) = octos_home {
        candidates.push(home.join("skills").join("mofa-slides").join("styles"));

        if let Some(ref root_profile) = family_root_profile {
            candidates.push(
                home.join("profiles")
                    .join(root_profile)
                    .join("data")
                    .join("skills")
                    .join("mofa-slides")
                    .join("styles"),
            );
        }
    }

    candidates.into_iter().find(|candidate| candidate.is_dir())
}

/// Shared buffer of outbound messages from inactive sessions, keyed by session key string.
/// Flushed when the user switches to that session via `/s`.
pub type PendingMessages = Arc<Mutex<HashMap<String, Vec<OutboundMessage>>>>;

/// Shared lookup table for session-scoped background task supervisors.
#[derive(Default, Clone)]
pub struct SessionTaskQueryStore {
    supervisors: Arc<StdMutex<HashMap<String, SessionTaskQueryEntry>>>,
}

struct SessionTaskQueryEntry {
    supervisor: Weak<TaskSupervisor>,
    data_dir: PathBuf,
}

fn task_response_path(data_dir: &Path, path: &str) -> String {
    octos_bus::file_handle::encode_profile_file_handle(data_dir, Path::new(path))
        .unwrap_or_else(|| path.to_string())
}

fn sanitize_task_for_response(
    data_dir: &Path,
    task: &octos_agent::BackgroundTask,
) -> serde_json::Value {
    serde_json::json!({
        "id": task.id,
        "tool_name": task.tool_name,
        "tool_call_id": task.tool_call_id,
        "status": task.status,
        "started_at": task.started_at,
        "updated_at": task.updated_at,
        "completed_at": task.completed_at,
        "runtime_state": task.runtime_state,
        "runtime_detail": task.runtime_detail,
        "output_files": task.output_files.iter().map(|path| task_response_path(data_dir, path)).collect::<Vec<_>>(),
        "error": task.error,
        "session_key": task.session_key,
    })
}

impl SessionTaskQueryStore {
    pub fn register(
        &self,
        session_key: &SessionKey,
        supervisor: &Arc<TaskSupervisor>,
        data_dir: &Path,
    ) {
        let mut guard = self.supervisors.lock().unwrap_or_else(|e| e.into_inner());
        guard.insert(
            session_key.to_string(),
            SessionTaskQueryEntry {
                supervisor: Arc::downgrade(supervisor),
                data_dir: data_dir.to_path_buf(),
            },
        );
    }

    pub fn query_json(&self, session_key: &str) -> serde_json::Value {
        let upgraded = {
            let mut guard = self.supervisors.lock().unwrap_or_else(|e| e.into_inner());
            match guard.get(session_key).and_then(|entry| {
                entry
                    .supervisor
                    .upgrade()
                    .map(|supervisor| (supervisor, entry.data_dir.clone()))
            }) {
                Some(entry) => Some(entry),
                None => {
                    guard.remove(session_key);
                    None
                }
            }
        };

        match upgraded {
            Some((supervisor, data_dir)) => serde_json::Value::Array(
                supervisor
                    .get_tasks_for_session(session_key)
                    .iter()
                    .map(|task| sanitize_task_for_response(&data_dir, task))
                    .collect(),
            ),
            None => serde_json::json!([]),
        }
    }
}

fn system_notice_metadata(sender_user_id: Option<&str>) -> serde_json::Value {
    sender_user_id
        .map(|uid| serde_json::json!({ METADATA_SENDER_USER_ID: uid }))
        .unwrap_or_else(|| serde_json::json!({}))
}

fn git_turn_summary(content: &str) -> String {
    let compact = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        "agent turn update".to_string()
    } else {
        compact
    }
}

fn merge_attachment_prompt_summaries(
    existing: Option<String>,
    incoming: Option<String>,
) -> Option<String> {
    match (existing, incoming) {
        (Some(mut existing), Some(incoming)) => {
            if !incoming.is_empty() {
                if !existing.is_empty() {
                    existing.push_str("\n\n");
                }
                existing.push_str(&incoming);
            }
            Some(existing)
        }
        (Some(existing), None) => Some(existing),
        (None, Some(incoming)) => Some(incoming),
        (None, None) => None,
    }
}

fn merge_optional_text(existing: Option<String>, incoming: Option<String>) -> Option<String> {
    match (existing, incoming) {
        (Some(mut existing), Some(incoming)) => {
            if !incoming.is_empty() {
                if !existing.is_empty() {
                    existing.push_str("\n\n");
                }
                existing.push_str(&incoming);
            }
            Some(existing)
        }
        (Some(existing), None) => Some(existing),
        (None, Some(incoming)) => Some(incoming),
        (None, None) => None,
    }
}

async fn snapshot_workspace_turn_for_path(
    session_key: &SessionKey,
    workspace_root: std::path::PathBuf,
    turn_summary: &str,
) -> Option<String> {
    let turn_summary = git_turn_summary(turn_summary);

    match tokio::task::spawn_blocking(move || {
        octos_agent::snapshot_workspace_turn(&workspace_root, &turn_summary)
    })
    .await
    {
        Ok(Ok(report)) => {
            if !report.committed.is_empty() {
                info!(
                    session = %session_key,
                    repos = ?report.committed,
                    "workspace turn snapshot committed"
                );
            }
            if report.enforced_failures.is_empty() && report.validation_failures.is_empty() {
                return None;
            }

            if !report.validation_failures.is_empty() {
                warn!(
                    session = %session_key,
                    failures = ?report.validation_failures,
                    "workspace contract validation failed"
                );
            }

            let enforcement_notice = if report.enforced_failures.is_empty() {
                None
            } else {
                let repo_labels = report
                    .enforced_failures
                    .iter()
                    .map(|failure| failure.repo_label.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let first_error = report
                    .enforced_failures
                    .first()
                    .map(|failure| failure.error.as_str())
                    .unwrap_or("unknown error");
                warn!(
                    session = %session_key,
                    failures = ?report.enforced_failures,
                    "workspace turn snapshot enforcement failed"
                );
                Some(format!(
                    "Workspace versioning failed for {repo_labels}. Turn snapshot was not recorded.\nError: {first_error}"
                ))
            };

            let validation_notice = if report.validation_failures.is_empty() {
                None
            } else {
                let failures = report
                    .validation_failures
                    .iter()
                    .map(|failure| {
                        format!(
                            "{} [{}] {}: {}",
                            failure.repo_label,
                            match failure.phase {
                                octos_agent::WorkspaceValidationPhase::TurnEnd => "turn_end",
                                octos_agent::WorkspaceValidationPhase::Completion => "completion",
                            },
                            failure.check,
                            failure.reason
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Some(format!("Workspace contract validation failed:\n{failures}"))
            };

            merge_optional_text(enforcement_notice, validation_notice)
        }
        Ok(Err(error)) => {
            warn!(
                session = %session_key,
                error = %error,
                "workspace turn snapshot failed"
            );
            Some(format!(
                "Workspace versioning failed. Turn snapshot was not recorded.\nError: {error}"
            ))
        }
        Err(error) => {
            warn!(
                session = %session_key,
                error = %error,
                "workspace turn snapshot task failed"
            );
            Some(format!(
                "Workspace versioning task failed. Turn snapshot was not recorded.\nError: {error}"
            ))
        }
    }
}

async fn emit_workspace_snapshot_notice(
    out_tx: &mpsc::Sender<OutboundMessage>,
    channel: &str,
    chat_id: &str,
    reply_to: Option<String>,
    sender_user_id: Option<&str>,
    content: String,
) {
    let _ = out_tx
        .send(OutboundMessage {
            channel: channel.to_string(),
            chat_id: chat_id.to_string(),
            content,
            reply_to,
            media: vec![],
            metadata: system_notice_metadata(sender_user_id),
        })
        .await;
}

// ── Messages ────────────────────────────────────────────────────────────────

/// Messages dispatched to a session actor.
pub enum ActorMessage {
    /// A user message to process.
    Inbound {
        message: InboundMessage,
        image_media: Vec<String>,
        attachment_media: Vec<String>,
        attachment_prompt: Option<String>,
    },
    /// Result from a background subagent task — injected as a system message
    /// into the conversation without triggering an extra LLM call.
    BackgroundResult {
        /// Task identifier for attribution.
        task_label: String,
        /// The subagent's final output.
        content: String,
        /// Delivery semantics for this result.
        kind: BackgroundResultKind,
        /// Media files attached to this terminal background result.
        media: Vec<String>,
        /// Completion acknowledgment for durable persistence.
        ack: Option<oneshot::Sender<bool>>,
    },
    /// Background task status changed — push to SSE.
    TaskStatusChanged {
        /// Serialized JSON of the BackgroundTask.
        task_json: String,
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
    /// Profile system prompt override — preserved for respawn on actor death.
    system_prompt_override: Option<String>,
    /// Sender user ID for outbound identity assertion — preserved for respawn.
    sender_user_id: Option<String>,
    /// Profile-specific factory cache key for respawn after actor death.
    factory_profile_id: Option<String>,
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
    factory: Arc<ActorFactory>,
    profile_factories: HashMap<String, Arc<ActorFactory>>,
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
            factory: Arc::new(factory),
            profile_factories: HashMap::new(),
            semaphore,
            out_tx,
            pending_messages,
        }
    }

    pub fn register_profile_factory(
        &mut self,
        profile_id: impl Into<String>,
        factory: ActorFactory,
    ) {
        self.profile_factories
            .insert(profile_id.into(), Arc::new(factory));
    }

    pub fn has_profile_factory(&self, profile_id: &str) -> bool {
        self.profile_factories.contains_key(profile_id)
    }

    fn actor_key(session_key: &SessionKey, profile_id: Option<&str>) -> String {
        if session_key.profile_id().is_some() {
            session_key.to_string()
        } else {
            format!("{}:{}", profile_id.unwrap_or(MAIN_PROFILE_ID), session_key)
        }
    }

    fn resolve_factory(&self, profile_id: Option<&str>) -> (Arc<ActorFactory>, Option<String>) {
        if let Some(profile_id) = profile_id {
            if let Some(factory) = self.profile_factories.get(profile_id) {
                return (factory.clone(), Some(profile_id.to_string()));
            }
        }
        (self.factory.clone(), None)
    }

    /// Route an inbound message to the correct actor, creating one if needed.
    pub async fn dispatch(&mut self, params: DispatchParams<'_>) {
        let DispatchParams {
            message,
            image_media,
            attachment_media,
            attachment_prompt,
            session_key,
            reply_channel,
            reply_chat_id,
            status_indicator,
            profile_id,
            system_prompt_override,
            sender_user_id,
        } = params;
        let key_str = Self::actor_key(&session_key, profile_id);

        // If actor exists but has finished (idle-timeout/panic), remove it
        if let Some(handle) = self.actors.get(&key_str) {
            if handle.is_finished() {
                self.actors.remove(&key_str);
            }
        }

        // Create actor if needed
        if !self.actors.contains_key(&key_str) {
            let (factory, factory_profile_id) = self.resolve_factory(profile_id);
            let (tx, join_handle) = factory.spawn(SpawnParams {
                session_key: session_key.clone(),
                channel: reply_channel,
                chat_id: reply_chat_id,
                semaphore: self.semaphore.clone(),
                status_indicator: status_indicator.clone(),
                system_prompt_override: system_prompt_override.clone(),
                sender_user_id: sender_user_id.clone(),
            });
            self.actors.insert(
                key_str.clone(),
                ActorHandle {
                    tx,
                    created_at: Instant::now(),
                    join_handle,
                    system_prompt_override,
                    sender_user_id: sender_user_id.clone(),
                    factory_profile_id,
                },
            );
        }

        let handle = self.actors.get(&key_str).unwrap();
        let actor_msg = ActorMessage::Inbound {
            message,
            image_media,
            attachment_media,
            attachment_prompt,
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
                        metadata: system_notice_metadata(sender_user_id.as_deref()),
                    })
                    .await;
                // Now block until space is available
                let handle = self.actors.get(&key_str).unwrap();
                let _ = handle.tx.send(actor_msg).await;
            }
            Err(mpsc::error::TrySendError::Closed(actor_msg)) => {
                // Actor died — retrieve profile overrides, then respawn
                let dead = self.actors.remove(&key_str);
                let (prompt_override, uid_override, factory_profile_id) = dead
                    .map(|h| {
                        (
                            h.system_prompt_override,
                            h.sender_user_id,
                            h.factory_profile_id,
                        )
                    })
                    .unwrap_or((None, None, None));
                let factory = factory_profile_id
                    .as_deref()
                    .and_then(|pid| self.profile_factories.get(pid))
                    .cloned()
                    .unwrap_or_else(|| self.factory.clone());
                let (tx, join_handle) = factory.spawn(SpawnParams {
                    session_key,
                    channel: reply_channel,
                    chat_id: reply_chat_id,
                    semaphore: self.semaphore.clone(),
                    status_indicator,
                    system_prompt_override: prompt_override.clone(),
                    sender_user_id: uid_override.clone(),
                });
                let _ = tx.send(actor_msg).await;
                self.actors.insert(
                    key_str,
                    ActorHandle {
                        tx,
                        created_at: Instant::now(),
                        join_handle,
                        system_prompt_override: prompt_override,
                        sender_user_id: uid_override,
                        factory_profile_id,
                    },
                );
            }
        }
    }

    /// Returns the dispatch keys of all active actors (for testing).
    #[cfg(test)]
    pub fn actor_keys(&self) -> Vec<String> {
        self.actors.keys().cloned().collect()
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
        let scoped_suffix = format!(":{session_key}");
        let handles: Vec<_> = self
            .actors
            .iter()
            .filter(|(key, _)| key.as_str() == session_key || key.ends_with(&scoped_suffix))
            .map(|(_, handle)| handle.tx.clone())
            .collect();
        for tx in handles {
            let _ = tx.send(ActorMessage::Cancel).await;
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
    /// Strong-only provider chain for slides sessions (kimi + deepseek + minimax).
    pub llm_strong: Arc<dyn LlmProvider>,
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
    pub cron_service: Option<Arc<octos_bus::CronService>>,
    pub tool_registry_factory: Arc<dyn ToolRegistryFactory + Send + Sync>,
    pub pipeline_factory: Option<Arc<dyn PipelineToolFactory + Send + Sync>>,
    pub max_history: Arc<std::sync::atomic::AtomicUsize>,
    pub idle_timeout: Duration,
    pub session_timeout: Duration,
    pub shutdown: Arc<AtomicBool>,
    /// Working directory for SpawnTool (shared profile-level cwd).
    pub cwd: std::path::PathBuf,
    /// Sandbox config — used to create per-user sandbox instances.
    pub sandbox_config: octos_agent::SandboxConfig,
    /// Provider policy for SpawnTool and PipelineTool.
    pub provider_policy: Option<ToolPolicy>,
    /// Worker system prompt for SpawnTool subagents.
    pub worker_prompt: Option<String>,
    /// Provider router for SpawnTool and PipelineTool.
    pub provider_router: Option<Arc<ProviderRouter>>,
    /// Optional embedder for episodic memory recall.
    pub embedder: Option<Arc<dyn EmbeddingProvider>>,
    /// Active session store — used to check if a session is currently active.
    pub active_sessions: Arc<RwLock<ActiveSessionStore>>,
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
    /// Plugin directories for SpawnTool subagents to load plugin tools.
    pub plugin_dirs: Vec<std::path::PathBuf>,
    /// Extra environment variables for plugin processes in subagents.
    pub plugin_extra_env: Vec<(String, String)>,
    /// Session-scoped background task lookup for API inspection.
    pub task_query_store: SessionTaskQueryStore,
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
        sandbox: Box<dyn octos_agent::Sandbox>,
    ) -> ToolRegistry;
}

/// Trait for creating per-session pipeline tool instances.
pub trait PipelineToolFactory: Send + Sync {
    fn create(&self) -> Arc<dyn octos_agent::tools::Tool>;
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
        sandbox: Box<dyn octos_agent::Sandbox>,
    ) -> ToolRegistry {
        // Re-bind cwd-bound tools to the per-user workspace while
        // preserving non-cwd tools (web_search, browser, MCP, plugins, etc.)
        self.base.rebind_cwd(workspace, sandbox)
    }
}

impl ActorFactory {
    /// Spawn a new session actor, returning its inbox sender and join handle.
    fn spawn(&self, params: SpawnParams<'_>) -> (mpsc::Sender<ActorMessage>, JoinHandle<()>) {
        let SpawnParams {
            session_key,
            channel,
            chat_id,
            semaphore,
            status_indicator,
            system_prompt_override,
            sender_user_id,
        } = params;
        let (tx, rx) = mpsc::channel(ACTOR_INBOX_SIZE);

        // Create a per-session proxy channel. ALL outbound messages from this
        // session (tools, final reply, errors) flow through proxy_tx. A
        // forwarding task checks whether this session is active and either
        // delivers immediately or buffers for later.
        let (proxy_tx, proxy_rx) = mpsc::channel::<OutboundMessage>(64);

        // Per-session tools — they write to proxy_tx, not the real out_tx
        let message_tool = MessageTool::with_context(proxy_tx.clone(), channel, chat_id);

        // Build per-user workspace directory for file isolation.
        // Each user's tools are restricted to their own workspace via
        // resolve_path() (application-level) and sandbox-exec SBPL (kernel-level on macOS).
        let encoded_base = octos_bus::session::encode_path_component(session_key.base_key());
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
        // Create the per-actor session handle early so we can derive the
        // background task ledger path before any worker can mutate state.
        let session_handle = SessionHandle::open(&self.data_dir, &session_key);
        let task_state_path = session_handle.task_state_path();
        let session_handle = Arc::new(Mutex::new(session_handle));
        let session_policy_path = workspace_policy_path(&user_workspace);
        let desired_session_policy = WorkspacePolicy::for_session();
        match read_workspace_policy(&user_workspace) {
            Ok(Some(mut existing_policy)) => {
                let mut updated = false;
                for (name, pattern) in &desired_session_policy.artifacts.entries {
                    if !existing_policy.artifacts.entries.contains_key(name) {
                        existing_policy
                            .artifacts
                            .entries
                            .insert(name.clone(), pattern.clone());
                        updated = true;
                    }
                }
                for (name, task) in &desired_session_policy.spawn_tasks {
                    if !existing_policy.spawn_tasks.contains_key(name) {
                        existing_policy
                            .spawn_tasks
                            .insert(name.clone(), task.clone());
                        updated = true;
                    }
                }
                if updated {
                    if let Err(error) = write_workspace_policy(&user_workspace, &existing_policy) {
                        warn!(
                            session = %session_key,
                            path = %session_policy_path.display(),
                            "failed to upgrade session workspace policy: {error}"
                        );
                    }
                }
            }
            Ok(None) => {
                if let Err(error) = write_workspace_policy(&user_workspace, &desired_session_policy)
                {
                    warn!(
                        session = %session_key,
                        path = %session_policy_path.display(),
                        "failed to write session workspace policy: {error}"
                    );
                }
            }
            Err(error) => {
                warn!(
                    session = %session_key,
                    path = %session_policy_path.display(),
                    "failed to read session workspace policy: {error}"
                );
            }
        }

        // send_file resolves relative paths against user_workspace (same as
        // write_file/read_file) so the LLM can write+send in one flow.
        // data_dir is an extra allowed directory for pipeline-generated files.
        let send_file_tool = SendFileTool::with_context(proxy_tx.clone(), channel, chat_id)
            .with_base_dir(&user_workspace)
            .with_extra_allowed_dir(&self.data_dir);

        // Create tool registry with cwd-bound tools pointing to the per-user workspace.
        // A fresh sandbox is created per user so the SBPL profile restricts writes
        // to this user's workspace directory (kernel-enforced on macOS).
        let user_sandbox = octos_agent::create_sandbox(&self.sandbox_config);
        let mut tools = self
            .tool_registry_factory
            .create_registry_for_workspace(&user_workspace, user_sandbox);
        let supervisor = tools.supervisor();
        if let Err(error) = supervisor.enable_persistence(task_state_path) {
            warn!(
                session = %session_key,
                error = %error,
                "failed to enable task supervisor persistence"
            );
        }
        self.task_query_store
            .register(&session_key, &supervisor, &self.data_dir);
        tools.rebind_plugin_work_dirs(&user_workspace);
        tools.set_session_key(session_key.to_string());
        tools.register(CheckBackgroundTasksTool::new(
            supervisor.clone(),
            session_key.to_string(),
        ));
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
        .with_provider_policy(self.provider_policy.clone())
        .with_agent_config(self.agent_config.clone())
        .with_task_supervisor(supervisor.clone(), session_key.to_string());
        if let Some(ref prompt) = self.worker_prompt {
            spawn_tool = spawn_tool.with_worker_prompt(prompt.clone());
        }
        if let Some(ref router) = self.provider_router {
            spawn_tool = spawn_tool.with_provider_router(router.clone());
        }
        if !self.plugin_dirs.is_empty() {
            spawn_tool = spawn_tool
                .with_plugin_dirs(self.plugin_dirs.clone(), self.plugin_extra_env.clone());
        }

        // Wire direct background result injection (bypasses InboundMessage relay)
        let bg_tx = tx.clone();
        spawn_tool = spawn_tool.with_background_result_sender(Arc::new(
            move |payload: BackgroundResultPayload| {
                let tx = bg_tx.clone();
                Box::pin(async move {
                    let (ack_tx, ack_rx) = oneshot::channel();
                    tx.send(ActorMessage::BackgroundResult {
                        task_label: payload.task_label,
                        content: payload.content,
                        kind: payload.kind,
                        media: payload.media,
                        ack: Some(ack_tx),
                    })
                    .await
                    .is_ok()
                        && ack_rx.await.unwrap_or(false)
                })
            },
        ));

        tools.register(spawn_tool);

        // Wire background result sender for spawn_only tool lifecycle notifications
        let bg_tx2 = tx.clone();
        tools.set_background_result_sender(Arc::new(move |payload: BackgroundResultPayload| {
            let tx = bg_tx2.clone();
            Box::pin(async move {
                let (ack_tx, ack_rx) = oneshot::channel();
                tx.send(ActorMessage::BackgroundResult {
                    task_label: payload.task_label,
                    content: payload.content,
                    kind: payload.kind,
                    media: payload.media,
                    ack: Some(ack_tx),
                })
                .await
                .is_ok()
                    && ack_rx.await.unwrap_or(false)
            })
        }));

        // Wire supervisor on_change callback to push task status via SSE.
        // Uses try_send to avoid blocking the sync Mutex context.
        let status_tx = tx.clone();
        let task_data_dir = self.data_dir.clone();
        supervisor.set_on_change(move |task| {
            let task_json = sanitize_task_for_response(&task_data_dir, task);
            if let Ok(json) = serde_json::to_string(&task_json) {
                let _ = status_tx.try_send(ActorMessage::TaskStatusChanged { task_json: json });
            }
        });

        let cron_tool_ref = if let Some(ref cron_service) = self.cron_service {
            let cron_tool = Arc::new(CronTool::with_context(
                cron_service.clone(),
                channel,
                chat_id,
            ));
            tools.register_arc(cron_tool.clone());
            Some(cron_tool)
        } else {
            None
        };

        if let Some(ref pf) = self.pipeline_factory {
            let pt = pf.create();
            tools.register_arc(pt);
        }

        // Defer rarely-used per-session tools to keep active tool count low
        // for providers that choke on many tools (e.g. Dashscope).
        // Keep run_pipeline active — it's a core research tool.
        tools.defer(["spawn".to_string(), "cron".to_string()]);

        // For slides sessions, auto-activate media tools and use primary model
        // (bypasses adaptive router which may pick a weak model).
        let is_slides = session_key.topic().is_some_and(|t| t.starts_with("slides"));
        let is_site = session_key
            .topic()
            .is_some_and(|t| t == "site" || t.starts_with("site "));
        if is_slides {
            tools.activate("group:media");

            // Scaffold slides project INTO the workspace so file tools
            // (read_file, write_file, mofa_slides) all resolve the same paths.
            // The earlier scaffold in gateway_dispatcher writes to data_dir
            // which is unreachable from the sandboxed workspace.
            let topic = session_key.topic().unwrap_or("slides");
            let project_name = topic.strip_prefix("slides").unwrap_or("").trim();
            let project_name = if project_name.is_empty() {
                "untitled"
            } else {
                project_name
            };
            if let Err(error) =
                crate::project_templates::scaffold_slides_project(&user_workspace, project_name)
            {
                warn!(session = %session_key, "slides scaffold failed in workspace: {error}");
            }

            // Copy built-in style templates into workspace/styles/ so the
            // agent's glob("styles/*.toml") can discover them.
            let builtin_styles = resolve_builtin_slides_styles_dir(&self.data_dir);
            let ws_styles = user_workspace.join("styles");
            if let Some(builtin_styles) = builtin_styles {
                std::fs::create_dir_all(&ws_styles).ok();
                if let Ok(entries) = std::fs::read_dir(&builtin_styles) {
                    for entry in entries.flatten() {
                        let src = entry.path();
                        if src.extension().is_some_and(|e| e == "toml") {
                            let dst = ws_styles.join(entry.file_name());
                            // Don't overwrite custom styles the user created
                            if !dst.exists() {
                                std::fs::copy(&src, &dst).ok();
                            }
                        }
                    }
                }
                let cyberpunk_alias = ws_styles.join("cyberpunk-neon.toml");
                let blade_runner = ws_styles.join("nb-br.toml");
                if !cyberpunk_alias.exists() && blade_runner.is_file() {
                    std::fs::copy(&blade_runner, &cyberpunk_alias).ok();
                }
            } else {
                warn!(
                    session = %session_key,
                    data_dir = %self.data_dir.display(),
                    "builtin mofa-slides styles directory not found"
                );
            }
        }
        let slides_generation_available = !is_slides || tools.get("mofa_slides").is_some();

        if is_site {
            let topic = session_key.topic().unwrap_or("site");
            let profile_id = session_key.profile_id().unwrap_or(MAIN_PROFILE_ID);
            if let Err(error) = crate::project_templates::scaffold_site_project(
                &user_workspace,
                profile_id,
                session_key.chat_id(),
                topic,
                &self.data_dir,
            ) {
                warn!(session = %session_key, "site scaffold failed in workspace: {error}");
            }
        }

        // Slides sessions use the strong-only provider chain — failover
        // between kimi/deepseek/minimax only, excluding weak providers that
        // hang on 30+ tools. Normal sessions use the full adaptive router.
        let session_llm = if is_slides {
            self.llm_strong.clone()
        } else {
            self.llm.clone()
        };
        let agent_id = AgentId::new(format!("session-{}", session_key));
        let has_deferred = tools.has_deferred();
        let mut system_prompt = system_prompt_override.unwrap_or_else(|| {
            self.system_prompt
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        });
        if is_slides && !slides_generation_available {
            system_prompt.push_str(
                "\n\n## Slides Generation Availability\n\n\
                 `mofa_slides` is not available on this host. You may still design and edit slide projects, \
                 but you must tell the user that PPTX/image generation is unavailable here. \
                 Do NOT retry generation via shell, run_pipeline, or alternative binaries.",
            );
        }
        if has_deferred {
            let groups = tools.deferred_groups();
            let mut tool_names = Vec::new();
            for (name, _desc, _count) in &groups {
                if let Some(info) = octos_agent::tools::policy::TOOL_GROUPS
                    .iter()
                    .find(|g| g.name == name)
                {
                    tool_names.extend(info.tools.iter().copied());
                }
            }
            let template = include_str!("../../octos-agent/src/prompts/deferred_tools.txt");
            system_prompt.push_str(&template.replace("{tool_list}", &tool_names.join(", ")));
        }

        // Per-session cancellation flag: shared with the agent so that
        // interrupt mode can stop a running agent loop mid-iteration.
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut agent = Agent::new(agent_id, session_llm, tools, self.memory.clone())
            .with_config(self.agent_config.clone())
            .with_reporter(Arc::new(octos_agent::SilentReporter))
            .with_shutdown(cancelled.clone())
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

        // Wire the activate_tools back-reference now that tools are in Arc
        agent.wire_activate_tools();

        // Load per-user status configuration
        let user_status_config = UserStatusConfig::load(&self.data_dir, session_key.base_key());

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
            sender_user_id: sender_user_id.clone(),
            user_status_config,
            data_dir: self.data_dir.clone(),
            max_history: self.max_history.clone(),
            idle_timeout: self.idle_timeout,
            session_timeout: self.session_timeout,
            semaphore,
            global_shutdown: self.shutdown.clone(),
            cancelled,
            queue_mode: self.queue_mode,
            responsiveness: ResponsivenessObserver::new(),
            adaptive_router: self.adaptive_router.clone(),
            memory_store: self.memory_store.clone(),
            active_overflow_tasks: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            overflow_cancelled: Arc::new(AtomicBool::new(false)),
            active_sessions: self.active_sessions.clone(),
            user_workspace: user_workspace.clone(),
            cron_tool: cron_tool_ref,
        };

        // Spawn the outbound forwarding task — buffers messages from inactive sessions
        let fwd_session_key = session_key.clone();
        let fwd_out_tx = self.out_tx.clone();
        let fwd_active = self.active_sessions.clone();
        let fwd_pending = self.pending_messages.clone();
        let fwd_channel = channel.to_string();
        let fwd_chat_id = chat_id.to_string();
        tokio::spawn(outbound_forwarder(ForwarderParams {
            proxy_rx,
            out_tx: fwd_out_tx,
            session_key: fwd_session_key,
            channel: fwd_channel,
            chat_id: fwd_chat_id,
            active_sessions: fwd_active,
            pending_messages: fwd_pending,
            sender_user_id,
        }));

        let join_handle = tokio::spawn(actor.run());

        info!(session = %session_key, channel, chat_id, "spawned session actor");
        (tx, join_handle)
    }
}

/// Forwarding task: reads from the session's proxy channel and either delivers
/// messages directly (if this session is active) or buffers them.
async fn outbound_forwarder(params: ForwarderParams) {
    let ForwarderParams {
        mut proxy_rx,
        out_tx,
        session_key,
        channel,
        chat_id,
        active_sessions,
        pending_messages,
        sender_user_id,
    } = params;
    let my_topic = session_key.topic().unwrap_or("").to_string();
    let base_key = session_key.base_key().to_string();
    let key_str = session_key.to_string();

    while let Some(mut msg) = proxy_rx.recv().await {
        // Inject sender_user_id into outbound metadata so the channel
        // sends as the correct virtual user (appservice identity assertion).
        if let Some(ref uid) = sender_user_id {
            if let Some(obj) = msg.metadata.as_object_mut() {
                obj.insert(
                    METADATA_SENDER_USER_ID.to_string(),
                    serde_json::Value::String(uid.clone()),
                );
            }
        }
        let active_topic = active_sessions
            .read()
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
                // Replace the last buffered message with a truncation notice so the
                // user sees feedback when they switch to this session.
                if let Some(last) = buf.last_mut() {
                    last.content = format!(
                        "{}\n\n⚠️ Buffer full ({MAX_PENDING_PER_SESSION} messages). \
                         Some responses were dropped. Switch to this session to continue.",
                        last.content,
                    );
                }
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
                        metadata: system_notice_metadata(sender_user_id.as_deref()),
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
    sender_user_id: Option<String>,
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
    /// Cancellation flag for in-flight overflow tasks.
    /// Set when a slash command is handled so overflow responses don't
    /// interleave with command replies (GitHub issue #21).
    overflow_cancelled: Arc<AtomicBool>,
    /// Active session store — used to check if this session is currently active.
    /// When inactive, streaming edits are skipped so replies go through the
    /// proxy → pending buffer path and can be flushed on session switch.
    active_sessions: Arc<RwLock<ActiveSessionStore>>,
    /// Per-user workspace directory — the agent's sandboxed working directory.
    /// Media files uploaded by the user are copied here so read_file can access them.
    user_workspace: std::path::PathBuf,
    /// Per-session cron tool reference — updated with channel/chat_id on each message.
    cron_tool: Option<Arc<CronTool>>,
}

impl SessionActor {
    async fn snapshot_workspace_turn_if_needed(
        &self,
        turn_summary: &str,
        reply_to: Option<String>,
    ) {
        if let Some(notice) = snapshot_workspace_turn_for_path(
            &self.session_key,
            self.user_workspace.clone(),
            turn_summary,
        )
        .await
        {
            emit_workspace_snapshot_notice(
                &self.out_tx,
                &self.channel,
                &self.chat_id,
                reply_to,
                self.sender_user_id.as_deref(),
                notice,
            )
            .await;
        }
    }

    /// Check if this session is currently the active session for its chat.
    /// When inactive, streaming edits bypass the pending buffer, so we must
    /// skip streaming and let the reply go through the proxy path.
    async fn is_active(&self) -> bool {
        let my_topic = self.session_key.topic().unwrap_or("");
        let base_key = self.session_key.base_key();
        let active_topic = self
            .active_sessions
            .read()
            .await
            .get_active_topic(base_key)
            .to_string();
        my_topic == active_topic
    }

    async fn run(mut self) {
        loop {
            tokio::select! {
                msg = self.inbox.recv() => {
                    match msg {
                        Some(ActorMessage::Inbound {
                            message,
                            image_media,
                            attachment_media,
                            attachment_prompt,
                        }) => {
                            // Update cron tool context with current channel/chat_id
                            // so new cron jobs inherit the correct delivery target.
                            if let Some(ref cron) = self.cron_tool {
                                if !self.channel.is_empty() && !self.chat_id.is_empty() {
                                    cron.set_context(&self.channel, &self.chat_id);
                                }
                            }

                            // Check for abort trigger before processing
                            if octos_core::is_abort_trigger(&message.content) {
                                debug!(session = %self.session_key, "abort trigger detected");
                                self.cancelled.store(true, Ordering::Release);
                                let _ = self.out_tx.send(OutboundMessage {
                                    channel: self.channel.clone(),
                                    chat_id: self.chat_id.clone(),
                                    content: octos_core::abort_response(&message.content).to_string(),
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
                                // Cancel any in-flight overflow tasks so their
                                // responses don't preempt the command reply (#21).
                                self.overflow_cancelled.store(true, Ordering::Release);
                                // Send completion signal so the web client's SSE stream closes
                                if self.channel == "api" {
                                    let _ = self.out_tx.send(OutboundMessage {
                                        channel: self.channel.clone(),
                                        chat_id: self.chat_id.clone(),
                                        content: String::new(),
                                        reply_to: None,
                                        media: vec![],
                                        metadata: serde_json::json!({"_completion": true}),
                                    }).await;
                                }
                                continue;
                            }

                            // Drain any queued messages according to queue mode
                            let (
                                final_message,
                                final_media,
                                final_attachment_media,
                                final_attachment_prompt,
                            ) = self
                                .drain_queue(
                                    message,
                                    image_media,
                                    attachment_media,
                                    attachment_prompt,
                                )
                                .await;

                            // Copy non-image attachments into the agent workspace so
                            // tools can resolve them by filename without path hints.
                            let final_attachment_media =
                                self.copy_media_to_workspace(final_attachment_media);

                            // Use speculative path for API channel (web client) and
                            // Speculative queue mode. The speculative path spawns the
                            // agent call and handles overflow messages concurrently,
                            // so users aren't blocked during long tool calls (pipelines).
                            if self.queue_mode == QueueMode::Speculative || self.channel == "api" {
                                self.process_inbound_speculative(
                                    final_message,
                                    final_media,
                                    final_attachment_media,
                                    final_attachment_prompt,
                                )
                                .await;
                            } else {
                                self.process_inbound(
                                    final_message,
                                    final_media,
                                    final_attachment_media,
                                    final_attachment_prompt,
                                )
                                .await;
                            }
                        }
                        Some(ActorMessage::BackgroundResult {
                            task_label,
                            content,
                            kind,
                            media,
                            ack,
                        }) => {
                            let persisted = self
                                .handle_background_result(&task_label, &content, kind, media)
                                .await;
                            if let Some(ack) = ack {
                                let _ = ack.send(persisted);
                            }
                        }
                        Some(ActorMessage::TaskStatusChanged { task_json }) => {
                            // Push task status change to the web client via SSE
                            let _ = self.out_tx.send(octos_core::OutboundMessage {
                                channel: self.channel.clone(),
                                chat_id: self.chat_id.clone(),
                                content: String::new(),
                                reply_to: None,
                                media: vec![],
                                metadata: serde_json::json!({ "_task_status": task_json }),
                            }).await;
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
            "/reset" => {
                self.handle_reset_command().await;
                true
            }
            "/thinking" => {
                self.handle_thinking_command(&parts[1..]).await;
                true
            }
            _ => {
                // Unknown slash command — show help instead of passing to LLM
                self.send_reply(
                    "Unknown command. Available commands:\n\
                     /new [name] — start a new session\n\
                     /s [name] — switch to a session\n\
                     /sessions — list all sessions\n\
                     /back — return to default session\n\
                     /delete — delete current session\n\
                     /soul [text] — view or set persona\n\
                     /status — show agent status\n\
                     /adaptive — view adaptive routing\n\
                     /reset — reset session state\n\
                     /help — show this help",
                )
                .await;
                true
            }
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
        if let Err(e) = self.user_status_config.save(&self.data_dir, base_key) {
            warn!(error = %e, "failed to save user status config");
        }
    }

    /// `/reset` — reset session state for test isolation.
    ///
    /// Resets queue mode to default (collect) and clears conversation
    /// history for the current session. Does NOT touch the adaptive
    /// router — that's a gateway-level shared resource.
    async fn handle_reset_command(&mut self) {
        // Reset queue mode to default
        self.queue_mode = QueueMode::default();

        // Clear conversation history
        {
            let mut handle = self.session_handle.lock().await;
            if let Err(e) = handle.clear().await {
                warn!(error = %e, "failed to clear session history");
            }
        }

        self.send_reply("Reset: queue=collect, adaptive=off, history cleared.")
            .await;
    }

    /// `/thinking` — toggle display of model reasoning/thinking content.
    ///
    /// Usage:
    ///   /thinking          — show current state
    ///   /thinking on       — show thinking content in responses
    ///   /thinking off      — hide thinking content (default)
    async fn handle_thinking_command(&mut self, args: &[&str]) {
        match args.first().copied() {
            Some("on" | "true" | "1") => {
                self.user_status_config.show_thinking = true;
                self.send_reply("💭 Thinking display: **on** — reasoning content will be shown.")
                    .await;
            }
            Some("off" | "false" | "0") => {
                self.user_status_config.show_thinking = false;
                self.send_reply("💭 Thinking display: **off** — reasoning content will be hidden.")
                    .await;
            }
            None => {
                let state = if self.user_status_config.show_thinking {
                    "on"
                } else {
                    "off"
                };
                self.send_reply(&format!(
                    "💭 Thinking display: **{state}**\n\nUsage: `/thinking on` or `/thinking off`"
                ))
                .await;
            }
            _ => {
                self.send_reply("Usage: `/thinking on|off`").await;
            }
        }
        let base_key = self.session_key.base_key();
        if let Err(e) = self.user_status_config.save(&self.data_dir, base_key) {
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

        // Send completion marker so the API channel closes the SSE stream.
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
        attachment_media: Vec<String>,
        attachment_prompt: Option<String>,
    ) -> (InboundMessage, Vec<String>, Vec<String>, Option<String>) {
        match self.queue_mode {
            QueueMode::Followup | QueueMode::Speculative => {
                (message, image_media, attachment_media, attachment_prompt)
            }
            QueueMode::Collect => {
                let mut combined_content = message.content.clone();
                let mut combined_media = image_media;
                let mut combined_attachment_media = attachment_media;
                let mut combined_attachment_prompt = attachment_prompt;
                let mut count = 0u32;

                // Non-blocking drain of queued inbound messages
                loop {
                    match self.inbox.try_recv() {
                        Ok(ActorMessage::Inbound {
                            message: queued,
                            image_media: queued_media,
                            attachment_media: queued_attachment_media,
                            attachment_prompt: queued_attachment_prompt,
                        }) => {
                            if octos_core::is_abort_trigger(&queued.content) {
                                debug!(session = %self.session_key, "abort in queue, cancelling batch");
                                self.cancelled.store(true, Ordering::Release);
                                break;
                            }
                            count += 1;
                            combined_content
                                .push_str(&format!("\n---\nQueued #{count}: {}", queued.content));
                            combined_media.extend(queued_media);
                            combined_attachment_media.extend(queued_attachment_media);
                            combined_attachment_prompt = merge_attachment_prompt_summaries(
                                combined_attachment_prompt,
                                queued_attachment_prompt,
                            );
                        }
                        Ok(ActorMessage::BackgroundResult {
                            task_label,
                            content,
                            kind,
                            media,
                            ack,
                        }) => {
                            let persisted = self
                                .handle_background_result(&task_label, &content, kind, media)
                                .await;
                            if let Some(ack) = ack {
                                let _ = ack.send(persisted);
                            }
                        }
                        Ok(ActorMessage::TaskStatusChanged { .. }) => {
                            // Ignore in drain — status is pushed via the main loop
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
                (
                    msg,
                    combined_media,
                    combined_attachment_media,
                    combined_attachment_prompt,
                )
            }
            QueueMode::Steer | QueueMode::Interrupt => {
                // Coalescing delay: give rapid follow-up messages time to arrive
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                let mut latest_message = message;
                let mut latest_media = image_media;
                let mut latest_attachment_media = attachment_media;
                let mut latest_attachment_prompt = attachment_prompt;

                // Non-blocking drain: keep only the newest inbound message
                loop {
                    match self.inbox.try_recv() {
                        Ok(ActorMessage::Inbound {
                            message: queued,
                            image_media: queued_media,
                            attachment_media: queued_attachment_media,
                            attachment_prompt: queued_attachment_prompt,
                        }) => {
                            if octos_core::is_abort_trigger(&queued.content) {
                                debug!(session = %self.session_key, "abort in queue, cancelling");
                                self.cancelled.store(true, Ordering::Release);
                                break;
                            }
                            debug!(session = %self.session_key, "steer: replacing with newer message");
                            latest_message = queued;
                            latest_media = queued_media;
                            latest_attachment_media = queued_attachment_media;
                            latest_attachment_prompt = queued_attachment_prompt;
                        }
                        Ok(ActorMessage::BackgroundResult {
                            task_label,
                            content,
                            kind,
                            media,
                            ack,
                        }) => {
                            let persisted = self
                                .handle_background_result(&task_label, &content, kind, media)
                                .await;
                            if let Some(ack) = ack {
                                let _ = ack.send(persisted);
                            }
                        }
                        Ok(ActorMessage::TaskStatusChanged { .. }) => {
                            // Ignore in drain — status is pushed via the main loop
                        }
                        Ok(ActorMessage::Cancel) => {
                            self.cancelled.store(true, Ordering::Release);
                            break;
                        }
                        Err(_) => break,
                    }
                }
                (
                    latest_message,
                    latest_media,
                    latest_attachment_media,
                    latest_attachment_prompt,
                )
            }
        }
    }

    /// Inject a background task result into the conversation.
    ///
    /// For long results (>1000 chars), the full content is saved to the memory
    /// bank and only a summary is injected into session context.  The agent can
    /// retrieve the full report via `recall_memory("<slug>")`.
    /// Inject a background task result into the conversation context.
    ///
    /// Sends a preview notification directly to the user and injects the result
    /// into session history for subsequent turns.
    async fn deliver_background_notification(&self, content: String, media: Vec<String>) -> bool {
        let persisted = persist_assistant_message(
            &self.session_handle,
            &self.session_key,
            content.clone(),
            media.clone(),
        )
        .await;

        let metadata = match persisted.as_ref() {
            Some(persisted_message) => serde_json::json!({
                "_history_persisted": true,
                "_session_result": {
                    "seq": persisted_message.seq,
                    "role": "assistant",
                    "content": content.clone(),
                    "timestamp": persisted_message.timestamp.to_rfc3339(),
                    "media": media.clone(),
                }
            }),
            None => serde_json::json!({ "_history_persisted": false }),
        };

        let _ = self
            .out_tx
            .send(OutboundMessage {
                channel: self.channel.clone(),
                chat_id: self.chat_id.clone(),
                content,
                reply_to: None,
                media,
                metadata,
            })
            .await;

        persisted.is_some()
    }

    async fn handle_background_result(
        &self,
        task_label: &str,
        content: &str,
        kind: BackgroundResultKind,
        media: Vec<String>,
    ) -> bool {
        if kind == BackgroundResultKind::Notification {
            self.deliver_background_notification(content.to_string(), media)
                .await
        } else {
            self.inject_background_result(task_label, content).await
        }
    }

    async fn inject_background_result(&self, task_label: &str, content: &str) -> bool {
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
        let persisted = {
            let mut handle = self.session_handle.lock().await;
            if let Err(e) = handle.add_message(system_msg).await {
                warn!(session = %self.session_key, error = %e, "failed to inject background result");
                false
            } else {
                true
            }
        };

        // Send raw preview directly; the injected context is available on the
        // next turn without forcing a synthetic rewrite prompt.
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

        persisted
    }

    /// Copy media files from their original location (e.g. profile media_dir)
    /// into the agent's sandboxed `user_workspace` so that `read_file` and
    /// other cwd-bound tools can access them.  Returns the updated paths.
    fn copy_media_to_workspace(&self, media: Vec<String>) -> Vec<String> {
        media
            .into_iter()
            .map(|path| {
                let resolved = octos_bus::file_handle::resolve_upload_reference(&path)
                    .map(|candidate| candidate.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.clone());
                let src = std::path::Path::new(&resolved);
                if !src.exists() {
                    return resolved;
                }
                let Some(filename) = src.file_name() else {
                    return resolved;
                };
                let dest = self.user_workspace.join(filename);
                match std::fs::copy(src, &dest) {
                    Ok(_) => {
                        debug!(
                            session = %self.session_key,
                            src = %src.display(),
                            dest = %dest.display(),
                            "copied media file to workspace"
                        );
                        dest.to_string_lossy().into_owned()
                    }
                    Err(e) => {
                        warn!(
                            session = %self.session_key,
                            src = %src.display(),
                            error = %e,
                            "failed to copy media to workspace, using original path"
                        );
                        resolved
                    }
                }
            })
            .collect()
    }

    fn build_turn_attachment_context(
        &self,
        attachment_media: Vec<String>,
        attachment_prompt: Option<String>,
    ) -> TurnAttachmentContext {
        let mut audio_attachment_paths = Vec::new();
        let mut file_attachment_paths = Vec::new();
        for path in &attachment_media {
            if octos_bus::media::is_audio(path) {
                audio_attachment_paths.push(path.clone());
            } else {
                file_attachment_paths.push(path.clone());
            }
        }

        TurnAttachmentContext {
            attachment_paths: attachment_media,
            audio_attachment_paths,
            file_attachment_paths,
            prompt_summary: attachment_prompt,
        }
    }

    fn persisted_user_content(
        inbound: &InboundMessage,
        image_media: &[String],
        attachment_media: &[String],
    ) -> String {
        if inbound.content.is_empty() && !image_media.is_empty() {
            "[User sent an image]".to_string()
        } else if inbound.content.is_empty() && !attachment_media.is_empty() {
            "[User sent attachments]".to_string()
        } else {
            inbound.content.clone()
        }
    }

    fn forced_background_workflow_for_turn(
        &self,
        inbound: &InboundMessage,
        image_media: &[String],
        attachment_media: &[String],
    ) -> Option<ForcedBackgroundWorkflow> {
        if !image_media.is_empty() || !attachment_media.is_empty() {
            return None;
        }
        if self.channel == "system" {
            return None;
        }
        ForcedBackgroundWorkflow::detect(&inbound.content)
    }

    async fn maybe_start_forced_background_workflow(
        &self,
        inbound: &InboundMessage,
        image_media: &[String],
        attachment_media: &[String],
        attachment_prompt: Option<&str>,
        persisted_user_content: &str,
        reply_to: Option<String>,
    ) -> bool {
        let Some(workflow) =
            self.forced_background_workflow_for_turn(inbound, image_media, attachment_media)
        else {
            return false;
        };

        let mut task = inbound.content.clone();
        if let Some(prompt) = attachment_prompt.filter(|value| !value.trim().is_empty()) {
            task.push_str("\n\nAttachment context:\n");
            task.push_str(prompt);
        }

        let args = serde_json::json!({
            "task": task,
            "label": workflow.label(),
            "mode": "background",
            "allowed_tools": workflow.allowed_tools(),
            "additional_instructions": workflow.additional_instructions(),
        });

        let tool_registry = self.agent.tool_registry();
        let spawn_result = match tool_registry.execute("spawn", &args).await {
            Ok(result) if result.success => result,
            Ok(result) => {
                warn!(
                    session = %self.session_key,
                    workflow = workflow.label(),
                    error = %result.output,
                    "forced background spawn returned failure"
                );
                return false;
            }
            Err(error) => {
                warn!(
                    session = %self.session_key,
                    workflow = workflow.label(),
                    error = %error,
                    "forced background spawn failed"
                );
                return false;
            }
        };

        let user_msg = Message {
            role: MessageRole::User,
            content: persisted_user_content.to_string(),
            media: vec![],
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            timestamp: chrono::Utc::now(),
        };
        {
            let mut handle = self.session_handle.lock().await;
            let session = handle.get_or_create();
            if session.summary.is_none() && !persisted_user_content.trim().is_empty() {
                session.summary = Some(persisted_user_content.chars().take(100).collect());
            }
            if let Err(error) = handle.add_message(user_msg).await {
                warn!(session = %self.session_key, error = %error, "failed to persist user message for forced background workflow");
            }
        }

        let ack_content = workflow.ack_message().to_string();
        let persisted = persist_assistant_message(
            &self.session_handle,
            &self.session_key,
            ack_content.clone(),
            vec![],
        )
        .await;

        let _ = self
            .out_tx
            .send(OutboundMessage {
                channel: self.channel.clone(),
                chat_id: self.chat_id.clone(),
                content: ack_content,
                reply_to,
                media: vec![],
                metadata: serde_json::json!({
                    "_history_persisted": persisted,
                    "spawn_output": spawn_result.output,
                }),
            })
            .await;

        if self.channel == "api" {
            let bg_tasks = tool_registry
                .supervisor()
                .get_tasks_for_session(&self.session_key.to_string())
                .into_iter()
                .filter(|task| task.status.is_active())
                .map(|task| sanitize_task_for_response(&self.data_dir, &task))
                .collect::<Vec<_>>();

            let _ = self
                .out_tx
                .send(OutboundMessage {
                    channel: self.channel.clone(),
                    chat_id: self.chat_id.clone(),
                    content: String::new(),
                    reply_to: None,
                    media: vec![],
                    metadata: serde_json::json!({
                        "_completion": true,
                        "has_bg_tasks": !bg_tasks.is_empty(),
                        "bg_tasks": bg_tasks,
                    }),
                })
                .await;
        }

        true
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
        attachment_media: Vec<String>,
        attachment_prompt: Option<String>,
    ) {
        // Reset overflow cancellation from any prior command handling (#21).
        self.overflow_cancelled.store(false, Ordering::Release);

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

        let persisted_user_content =
            Self::persisted_user_content(&inbound, &image_media, &attachment_media);

        // ── Setup (needs &mut self briefly for permit + reporter) ────────

        let _permit = match self.semaphore.acquire().await {
            Ok(p) => p,
            Err(_) => return,
        };

        if self
            .maybe_start_forced_background_workflow(
                &inbound,
                &image_media,
                &attachment_media,
                attachment_prompt.as_deref(),
                &persisted_user_content,
                inbound_message_id.clone(),
            )
            .await
        {
            self.cancelled.store(false, Ordering::Release);
            return;
        }

        let max_history = self.max_history.load(Ordering::Acquire);

        // Save the primary user message to session history BEFORE spawning
        // so overflow reads see it in context (chronological ordering).
        let user_msg = Message {
            role: MessageRole::User,
            content: persisted_user_content,
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
                self.sender_user_id.clone(),
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

        // Drop the original stream_tx — the reporter and callback each hold their
        // own clones.  If we keep this alive, the stream forwarder will never see
        // channel-closed and the await at the end of this function deadlocks.
        drop(stream_tx);

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
                let op_updater = status_handle.as_ref().map(|h| h.operation_updater());
                Some(tokio::spawn(crate::stream_reporter::run_stream_forwarder(
                    stream_rx,
                    channel,
                    self.chat_id.clone(),
                    cancel_status,
                    status_msg_id,
                    Arc::clone(&self.active_sessions),
                    self.session_key.clone(),
                    self.sender_user_id.clone(),
                    op_updater,
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
        let attachments = self.build_turn_attachment_context(attachment_media, attachment_prompt);
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
                agent.process_message_tracked_with_attachments(
                    &content,
                    &history_for_agent,
                    media,
                    attachments,
                    &tracker,
                ),
            )
            .await;
            eprintln!(
                "[DEBUG] agent_task finished in {}ms, ok={}",
                start.elapsed().as_millis(),
                result.is_ok()
            );
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
                            self.agent.set_reporter(Arc::new(octos_agent::SilentReporter));
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
                        Some(ActorMessage::Inbound {
                            message,
                            image_media: _,
                            attachment_media: _,
                            attachment_prompt: _,
                        }) => {
                            if octos_core::is_abort_trigger(&message.content) {
                                self.cancelled.store(true, Ordering::Release);
                                self.send_reply(octos_core::abort_response(&message.content)).await;
                                continue;
                            }
                            // Check if this is a slash command — handle inline
                            // instead of spawning an overflow agent.
                            if message.content.trim().starts_with('/') {
                                overflow_commands.push(message);
                                continue;
                            }
                            let elapsed = started.elapsed();

                            if self.queue_mode == QueueMode::Interrupt {
                                // Interrupt mode: abort the primary agent task
                                // so the new message can be processed immediately.
                                info!(
                                    session = %self.session_key,
                                    elapsed_ms = elapsed.as_millis(),
                                    "interrupt: aborting primary task for new message"
                                );
                                agent_task.abort();
                                self.cancelled.store(true, Ordering::Release);

                                // Process the interrupting message as overflow
                                // (same as speculative, but the primary is now dead)
                                self.serve_overflow(&message, &overflow_history);
                                overflow_served = true;
                                continue;
                            }

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
                        Some(ActorMessage::BackgroundResult {
                            task_label,
                            content,
                            kind,
                            media,
                            ack,
                        }) => {
                            let persisted = self
                                .handle_background_result(&task_label, &content, kind, media)
                                .await;
                            if let Some(ack) = ack {
                                let _ = ack.send(persisted);
                            }
                        }
                        Some(ActorMessage::TaskStatusChanged { task_json }) => {
                            let _ = self.out_tx.send(octos_core::OutboundMessage {
                                channel: self.channel.clone(),
                                chat_id: self.chat_id.clone(),
                                content: String::new(),
                                reply_to: None,
                                media: vec![],
                                metadata: serde_json::json!({ "_task_status": task_json }),
                            }).await;
                        }
                        Some(ActorMessage::Cancel) => {
                            self.cancelled.store(true, Ordering::Release);
                        }
                        None => {
                            // All senders dropped — actor shutting down
                            self.agent.set_reporter(Arc::new(octos_agent::SilentReporter));
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
        for cmd_msg in &overflow_commands {
            self.try_handle_command(cmd_msg).await;
        }
        // If any deferred commands were processed, cancel in-flight overflow
        // tasks so their responses don't preempt command replies (#21).
        if !overflow_commands.is_empty() {
            self.overflow_cancelled.store(true, Ordering::Release);
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
            .set_reporter(Arc::new(octos_agent::SilentReporter));

        // Clear adaptive router status callback (stream_tx is being dropped)
        if let Some(ref router) = self.adaptive_router {
            router.set_status_callback(None);
        }

        // Wait for stream forwarder — but NOT for API channel.
        // For API channel, the forwarder blocks on rx.recv() which requires
        // _completion to close the SSE sender. Since _completion is sent after
        // this function's match block, awaiting the forwarder here would deadlock.
        let stream_result = if self.channel == "api" {
            // Drop the forwarder handle — it will finish on its own when _completion
            // arrives and closes the SSE sender.
            drop(stream_forwarder);
            None
        } else if let Some(handle) = stream_forwarder {
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
        let supervisor = self.agent.tool_registry().supervisor();
        let bg_tasks = supervisor.task_count();
        let all_tasks = supervisor.get_all_tasks();
        let had_bg_tasks = !all_tasks.is_empty(); // any task was spawned, even if completed
        let bg_task_details: Vec<_> = supervisor.get_active_tasks();
        if !all_tasks.is_empty() {
            for t in &all_tasks {
                info!(
                    session = %self.session_key,
                    task_id = %t.id,
                    tool = %t.tool_name,
                    status = ?t.status,
                    files = ?t.output_files,
                    error = ?t.error,
                    "task supervisor report"
                );
            }
        }
        let completion_meta = match &agent_result {
            Ok(Ok(cr)) => {
                info!(session = %self.session_key, messages = cr.messages.len(), content_len = cr.content.len(), bg_tasks, "agent completed, saving messages");
                let provider_metadata = cr.provider_metadata.clone();
                let model_label = provider_metadata
                    .as_ref()
                    .map(|meta| meta.display_label())
                    .unwrap_or_else(|| {
                        format!("{}/{}", self.agent.provider_name(), self.agent.model_id())
                    });
                serde_json::json!({
                    "_completion": true,
                    "model": model_label,
                    "provider": provider_metadata.as_ref().map(|meta| meta.provider.clone()),
                    "model_id": provider_metadata.as_ref().map(|meta| meta.model.clone()),
                    "endpoint": provider_metadata.and_then(|meta| meta.endpoint),
                    "tokens_in": cr.token_usage.input_tokens,
                    "tokens_out": cr.token_usage.output_tokens,
                    "duration_s": llm_latency.as_secs_f64().round() as u64,
                    "has_bg_tasks": had_bg_tasks,
                    "bg_tasks": bg_task_details,
                })
            }
            Ok(Err(e)) => {
                warn!(session = %self.session_key, error = %e, "agent returned error");
                serde_json::json!({"_completion": true, "has_bg_tasks": had_bg_tasks, "bg_tasks": bg_task_details})
            }
            Err(e) => {
                warn!(session = %self.session_key, error = %e, "agent timed out");
                serde_json::json!({"_completion": true, "has_bg_tasks": had_bg_tasks, "bg_tasks": bg_task_details})
            }
        };
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

                    // The agent's ConversationResponse puts the final assistant
                    // text in `content` but may not include it as a Message in
                    // `messages` (EndTurn returns early without appending).
                    // Persist it explicitly so session history is complete.
                    if !conv_response.content.is_empty() {
                        let assistant_msg = Message {
                            role: MessageRole::Assistant,
                            content: conv_response.content.clone(),
                            media: vec![],
                            tool_calls: None,
                            tool_call_id: None,
                            reasoning_content: conv_response.reasoning_content.clone(),
                            timestamp: chrono::Utc::now(),
                        };
                        if let Err(e) = handle.add_message(assistant_msg).await {
                            warn!(session = %self.session_key, error = %e, "failed to persist assistant reply");
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

                // Auto-deliver report files produced by the agent (e.g. from run_pipeline).
                // This ensures the file reaches the user's channel (Telegram, web, etc.)
                // without relying on the LLM to call send_file within its token budget.
                if conv_response.files_modified.is_empty() {
                    tracing::debug!(session = %self.session_key, "no files_modified in conv_response");
                } else {
                    tracing::info!(
                        session = %self.session_key,
                        files = ?conv_response.files_modified.iter().map(|f| f.display().to_string()).collect::<Vec<_>>(),
                        "conv_response has files_modified"
                    );
                }
                for file in &conv_response.files_modified {
                    if file.extension().and_then(|e| e.to_str()) == Some("md") {
                        // Resolve relative paths to absolute so the file URL works
                        let abs_file = if file.is_relative() {
                            std::fs::canonicalize(file)
                                .or_else(|_| std::fs::canonicalize(self.data_dir.join(file)))
                                .unwrap_or_else(|_| file.clone())
                        } else {
                            file.clone()
                        };
                        info!(
                            session = %self.session_key,
                            file = %abs_file.display(),
                            channel = %self.channel,
                            chat_id = %self.chat_id,
                            "auto-delivering report file"
                        );
                        let file_msg = OutboundMessage {
                            channel: self.channel.clone(),
                            chat_id: self.chat_id.clone(),
                            content: String::new(),
                            reply_to: None,
                            media: vec![abs_file.to_string_lossy().into_owned()],
                            metadata: serde_json::json!({}),
                        };
                        if let Err(e) = self.out_tx.send(file_msg).await {
                            warn!(session = %self.session_key, error = %e, "failed to auto-deliver report file");
                        }
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

                    // Prepend thinking content when show_thinking is enabled
                    let display_content = if self.user_status_config.show_thinking {
                        let prefix =
                            format_thinking_prefix(conv_response.reasoning_content.as_deref());
                        format!("{prefix}{display_content}")
                    } else {
                        display_content
                    };

                    // If overflow was served while this task ran, prepend a
                    // marker so the user knows this is a delayed result.
                    let display_content = if overflow_served {
                        format!("⬆️ Earlier task completed:\n\n{display_content}")
                    } else {
                        display_content
                    };

                    // Append annotation as last line for non-API channels
                    let display_content = if self.channel != "api" {
                        if let Some(model) = completion_meta.get("model").and_then(|v| v.as_str()) {
                            let tok_in = completion_meta
                                .get("tokens_in")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            let tok_out = completion_meta
                                .get("tokens_out")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            let secs = completion_meta
                                .get("duration_s")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            format!(
                                "{display_content}\n\n{}",
                                format_annotation(model, tok_in, tok_out, secs)
                            )
                        } else {
                            display_content
                        }
                    } else {
                        display_content
                    };

                    // Skip streaming edit when session is inactive — let the
                    // reply go through proxy → pending buffer for later flush.
                    let session_active = self.is_active().await;
                    let streamed = if session_active {
                        if let Some(ref sr) = stream_result {
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
                let content = format!("Error: {e}");
                let _ = persist_assistant_message(
                    &self.session_handle,
                    &self.session_key,
                    content.clone(),
                    vec![],
                )
                .await;
                let _ = self
                    .out_tx
                    .send(OutboundMessage {
                        channel: self.channel.clone(),
                        chat_id: self.chat_id.clone(),
                        content,
                        reply_to: inbound_message_id.clone(),
                        media: vec![],
                        metadata: serde_json::json!({}),
                    })
                    .await;
            }
            Err(_) => {
                tracing::error!(session = %self.session_key, "session processing timed out");
                let content = "Processing timed out. Please try again.".to_string();
                let _ = persist_assistant_message(
                    &self.session_handle,
                    &self.session_key,
                    content.clone(),
                    vec![],
                )
                .await;
                let _ = self
                    .out_tx
                    .send(OutboundMessage {
                        channel: self.channel.clone(),
                        chat_id: self.chat_id.clone(),
                        content,
                        reply_to: inbound_message_id.clone(),
                        media: vec![],
                        metadata: serde_json::json!({}),
                    })
                    .await;
            }
        }

        self.snapshot_workspace_turn_if_needed(&inbound.content, inbound_message_id.clone())
            .await;

        // Reset per-session cancellation flag so the next message starts fresh.
        // This must happen AFTER the agent finishes, so it has had a chance to
        // observe the shutdown signal during its iteration loop.
        self.cancelled.store(false, Ordering::Release);

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
                    metadata: completion_meta,
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
        let sender_user_id = self.sender_user_id.clone();
        let user_status_config = self.user_status_config.clone();
        let history = pre_primary_history.to_vec();
        let active_sessions = self.active_sessions.clone();
        let overflow_cancelled = Arc::clone(&self.overflow_cancelled);
        let user_workspace = self.user_workspace.clone();

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
                    sender_user_id.clone(),
                )
            });

            // ── Per-overflow stream reporter (own chat bubble) ──────────────
            let (stream_tx, stream_rx) = tokio::sync::mpsc::unbounded_channel();
            let overflow_reporter: Arc<dyn octos_agent::ProgressReporter> = Arc::new(
                crate::stream_reporter::ChannelStreamReporter::new(stream_tx),
            );

            // Spawn stream forwarder — edits its OWN message, not the primary's
            let stream_forwarder = if let Some(ref si) = status_indicator {
                let fwd_channel = Arc::clone(si.channel());
                let cancel_status = status_handle.as_ref().map(|h| Arc::clone(&h.cancelled));
                let status_msg_id = status_handle.as_ref().map(|h| Arc::clone(&h.status_msg_id));
                let op_updater = status_handle.as_ref().map(|h| h.operation_updater());
                Some(tokio::spawn(crate::stream_reporter::run_stream_forwarder(
                    stream_rx,
                    fwd_channel,
                    chat_id.clone(),
                    cancel_status,
                    status_msg_id,
                    active_sessions.clone(),
                    session_key.clone(),
                    sender_user_id.clone(),
                    op_updater,
                )))
            } else {
                drop(stream_rx);
                None
            };

            // ── Run agent with task-local reporter override ─────────────────
            let reporter_for_scope = overflow_reporter.clone();
            let result = octos_agent::TASK_REPORTER
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
                handle.await.ok()
            } else {
                None
            };

            // Stop status indicator (deletes the "✦ Thinking..." message)
            if let Some(handle) = status_handle {
                handle.stop().await;
            }

            // If a slash command was handled while this overflow task was
            // running, suppress the response so it doesn't preempt the
            // command reply (GitHub issue #21).
            if overflow_cancelled.load(Ordering::Acquire) {
                info!(
                    session = %session_key,
                    "overflow task cancelled by command, suppressing response"
                );
                if let Some(notice) =
                    snapshot_workspace_turn_for_path(&session_key, user_workspace.clone(), &content)
                        .await
                {
                    emit_workspace_snapshot_notice(
                        &out_tx,
                        &channel,
                        &chat_id,
                        overflow_reply_to.clone(),
                        sender_user_id.as_deref(),
                        notice,
                    )
                    .await;
                }
                // Still decrement and return — skip sending any reply.
                overflow_counter.fetch_sub(1, Ordering::Release);
                return;
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
                            reasoning_content: conv_response.reasoning_content.clone(),
                            timestamp: chrono::Utc::now(),
                        };
                        let _ = handle.add_message(final_reply).await;
                    }

                    let reply = strip_think_tags(&conv_response.content);
                    // Prepend thinking content when show_thinking is enabled
                    let reply = if user_status_config.show_thinking {
                        let prefix =
                            format_thinking_prefix(conv_response.reasoning_content.as_deref());
                        format!("{prefix}{reply}")
                    } else {
                        reply
                    };
                    // Check session activity — if inactive, skip streaming edit
                    // so the reply goes through proxy → pending buffer.
                    let session_active = {
                        let my_topic = session_key.topic().unwrap_or("");
                        let base_key = session_key.base_key();
                        let active_topic = active_sessions
                            .read()
                            .await
                            .get_active_topic(base_key)
                            .to_string();
                        my_topic == active_topic
                    };
                    let already_streamed = session_active
                        && stream_result
                            .as_ref()
                            .is_some_and(|sr| sr.message_id.is_some());

                    if !reply.trim().is_empty() && !already_streamed {
                        let _ = out_tx
                            .send(OutboundMessage {
                                channel: channel.clone(),
                                chat_id: chat_id.clone(),
                                content: reply,
                                reply_to: overflow_reply_to.clone(),
                                media: vec![],
                                metadata: serde_json::json!({}),
                            })
                            .await;
                    }
                }
                Ok(Err(e)) => {
                    tracing::error!(session = %session_key, error = %e, "overflow agent task failed");
                    let content = format!("Error: {e}");
                    let _ = persist_assistant_message(
                        &session_handle,
                        &session_key,
                        content.clone(),
                        vec![],
                    )
                    .await;
                    let _ = out_tx
                        .send(OutboundMessage {
                            channel: channel.clone(),
                            chat_id: chat_id.clone(),
                            content,
                            reply_to: overflow_reply_to.clone(),
                            media: vec![],
                            metadata: serde_json::json!({}),
                        })
                        .await;
                }
                Err(_) => {
                    let content = "Processing timed out.".to_string();
                    let _ = persist_assistant_message(
                        &session_handle,
                        &session_key,
                        content.clone(),
                        vec![],
                    )
                    .await;
                    let _ = out_tx
                        .send(OutboundMessage {
                            channel: channel.clone(),
                            chat_id: chat_id.clone(),
                            content,
                            reply_to: overflow_reply_to.clone(),
                            media: vec![],
                            metadata: serde_json::json!({}),
                        })
                        .await;
                }
            }

            if let Some(notice) =
                snapshot_workspace_turn_for_path(&session_key, user_workspace, &content).await
            {
                emit_workspace_snapshot_notice(
                    &out_tx,
                    &channel,
                    &chat_id,
                    overflow_reply_to.clone(),
                    sender_user_id.as_deref(),
                    notice,
                )
                .await;
            }
            // Decrement active overflow counter
            overflow_counter.fetch_sub(1, Ordering::Release);
        });
    }

    async fn process_inbound(
        &mut self,
        inbound: InboundMessage,
        image_media: Vec<String>,
        attachment_media: Vec<String>,
        attachment_prompt: Option<String>,
    ) {
        // Capture the platform message ID for reply threading
        let inbound_message_id = inbound.message_id.clone();

        // Acquire concurrency permit
        let _permit = match self.semaphore.acquire().await {
            Ok(p) => p,
            Err(_) => return, // semaphore closed
        };

        let persisted_user_content =
            Self::persisted_user_content(&inbound, &image_media, &attachment_media);

        // Get conversation history
        let max_history = self.max_history.load(Ordering::Acquire);
        let history: Vec<Message> = {
            let mut handle = self.session_handle.lock().await;
            let session = handle.get_or_create();
            session.get_history(max_history).to_vec()
        };

        if self
            .maybe_start_forced_background_workflow(
                &inbound,
                &image_media,
                &attachment_media,
                attachment_prompt.as_deref(),
                &persisted_user_content,
                inbound_message_id.clone(),
            )
            .await
        {
            self.cancelled.store(false, Ordering::Release);
            return;
        }

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
                self.sender_user_id.clone(),
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

        // Drop the original stream_tx — clones live in reporter + callback.
        // Without this, the stream forwarder await deadlocks.
        drop(stream_tx);

        // Set provider layer on the status composer
        if let Some(ref handle) = status_handle {
            handle.set_provider(self.agent.provider_name(), self.agent.model_id());
        }

        // Spawn stream forwarder task — edits a channel message as text arrives.
        // Only for channels that support message editing/streaming (Discord,
        // Telegram, Feishu, WeCom bot). Channels without edit support (Slack,
        // etc.) skip streaming to avoid sending duplicate messages.
        let stream_forwarder = if let Some(ref si) = self.status_indicator {
            let channel = Arc::clone(si.channel());
            if channel.supports_edit() {
                let cancel_status = status_handle.as_ref().map(|h| Arc::clone(&h.cancelled));
                let status_msg_id = status_handle.as_ref().map(|h| Arc::clone(&h.status_msg_id));
                let op_updater = status_handle.as_ref().map(|h| h.operation_updater());
                Some(tokio::spawn(crate::stream_reporter::run_stream_forwarder(
                    stream_rx,
                    channel,
                    self.chat_id.clone(),
                    cancel_status,
                    status_msg_id,
                    Arc::clone(&self.active_sessions),
                    self.session_key.clone(),
                    self.sender_user_id.clone(),
                    op_updater,
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
            self.agent.process_message_tracked_with_attachments(
                &inbound.content,
                &history,
                image_media,
                self.build_turn_attachment_context(attachment_media, attachment_prompt),
                &token_tracker,
            ),
        )
        .await;
        let llm_latency = llm_start.elapsed();
        eprintln!(
            "[DEBUG] process_inbound: agent returned in {}ms, ok={}",
            llm_latency.as_millis(),
            result.is_ok()
        );

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
            .set_reporter(Arc::new(octos_agent::SilentReporter));

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

        // Capture annotation data before match moves result
        let annotation_data: Option<(String, u32, u32, u64)> = if let Ok(Ok(ref cr)) = result {
            Some((
                cr.provider_metadata
                    .as_ref()
                    .map(|meta| meta.display_label())
                    .unwrap_or_else(|| {
                        format!("{}/{}", self.agent.provider_name(), self.agent.model_id())
                    }),
                cr.token_usage.input_tokens,
                cr.token_usage.output_tokens,
                llm_latency.as_secs(),
            ))
        } else {
            None
        };

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

                    let mut persisted_user_message = false;
                    for msg in &conv_response.messages {
                        let message_to_save =
                            if !persisted_user_message && msg.role == MessageRole::User {
                                persisted_user_message = true;
                                let mut sanitized = msg.clone();
                                sanitized.content = persisted_user_content.clone();
                                sanitized
                            } else {
                                msg.clone()
                            };
                        if let Err(e) = handle.add_message(message_to_save).await {
                            warn!(session = %self.session_key, role = ?msg.role, error = %e, "failed to persist message");
                        }
                    }

                    // The agent's ConversationResponse puts the final assistant
                    // text in `content` but may not include it as a Message in
                    // `messages` (EndTurn returns early without appending).
                    // Persist it explicitly so session history is complete.
                    if !conv_response.content.is_empty() {
                        let assistant_msg = Message {
                            role: MessageRole::Assistant,
                            content: conv_response.content.clone(),
                            media: vec![],
                            tool_calls: None,
                            tool_call_id: None,
                            reasoning_content: conv_response.reasoning_content.clone(),
                            timestamp: chrono::Utc::now(),
                        };
                        if let Err(e) = handle.add_message(assistant_msg).await {
                            warn!(session = %self.session_key, error = %e, "failed to persist assistant reply");
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

                    // Prepend thinking content when show_thinking is enabled
                    let display_content = if self.user_status_config.show_thinking {
                        let prefix =
                            format_thinking_prefix(conv_response.reasoning_content.as_deref());
                        format!("{prefix}{display_content}")
                    } else {
                        display_content
                    };

                    // Append annotation as last line for non-API channels
                    let display_content = if self.channel != "api" {
                        if let Some((ref model, tok_in, tok_out, secs)) = annotation_data {
                            format!(
                                "{display_content}\n\n{}",
                                format_annotation(model, tok_in as u64, tok_out as u64, secs)
                            )
                        } else {
                            display_content
                        }
                    } else {
                        display_content
                    };

                    // If stream forwarder already sent a message AND this session
                    // is active, do a final edit. When inactive, skip the edit so
                    // the reply goes through the proxy → pending buffer path.
                    let session_active = self.is_active().await;
                    let streamed = if session_active {
                        if let Some(ref sr) = stream_result {
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
                let content = format!("Error: {e}");
                let _ = persist_assistant_message(
                    &self.session_handle,
                    &self.session_key,
                    content.clone(),
                    vec![],
                )
                .await;
                let _ = self
                    .out_tx
                    .send(OutboundMessage {
                        channel: self.channel.clone(),
                        chat_id: self.chat_id.clone(),
                        content,
                        reply_to: inbound_message_id.clone(),
                        media: vec![],
                        metadata: serde_json::json!({}),
                    })
                    .await;
            }
            Err(_) => {
                tracing::error!(session = %self.session_key, "session processing timed out");
                let content = "Processing timed out. Please try again.".to_string();
                let _ = persist_assistant_message(
                    &self.session_handle,
                    &self.session_key,
                    content.clone(),
                    vec![],
                )
                .await;
                let _ = self
                    .out_tx
                    .send(OutboundMessage {
                        channel: self.channel.clone(),
                        chat_id: self.chat_id.clone(),
                        content,
                        reply_to: inbound_message_id.clone(),
                        media: vec![],
                        metadata: serde_json::json!({}),
                    })
                    .await;
            }
        }

        self.snapshot_workspace_turn_if_needed(&inbound.content, inbound_message_id.clone())
            .await;

        // Reset per-session cancellation flag so the next message starts fresh.
        self.cancelled.store(false, Ordering::Release);

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
/// Collapses runs of 3+ newlines left behind to avoid blank gaps.
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
    // Collapse runs of 3+ newlines (left behind after stripping) to double newline
    while result.contains("\n\n\n") {
        result = result.replace("\n\n\n", "\n\n");
    }
    result.trim().to_string()
}

/// Format token count with K suffix for readability (e.g. 22173 → "22.2K").
fn fmt_tokens(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}K", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Format annotation line: model · tokens in/out · duration
fn format_annotation(model: &str, tok_in: u64, tok_out: u64, secs: u64) -> String {
    format!(
        "_{model} · {in_} in · {out_} out · {secs}s_",
        in_ = fmt_tokens(tok_in),
        out_ = fmt_tokens(tok_out),
    )
}

/// Format reasoning/thinking content for display, prepended to the response.
/// Truncates long reasoning to avoid flooding the channel.
fn format_thinking_prefix(reasoning: Option<&str>) -> String {
    const MAX_THINKING_LEN: usize = 1000;
    match reasoning {
        Some(r) if !r.trim().is_empty() => {
            let trimmed = r.trim();
            let display = if trimmed.chars().count() > MAX_THINKING_LEN {
                let truncated: String = trimmed.chars().take(MAX_THINKING_LEN).collect();
                format!("{truncated}...")
            } else {
                trimmed.to_string()
            };
            format!("💭 *Thinking:*\n{display}\n\n---\n\n")
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use octos_llm::{AdaptiveConfig, ChatConfig, ChatResponse, StopReason, TokenUsage, ToolSpec};
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

    #[test]
    fn test_resolve_builtin_slides_styles_dir_falls_back_to_root_profile() {
        let dir = tempfile::TempDir::new().unwrap();
        let octos_home = dir.path().join(".octos");
        let current_data = octos_home
            .join("profiles")
            .join("dspfac--newsbot")
            .join("data");
        let root_styles = octos_home
            .join("profiles")
            .join("dspfac")
            .join("data")
            .join("skills")
            .join("mofa-slides")
            .join("styles");

        std::fs::create_dir_all(&current_data).unwrap();
        std::fs::create_dir_all(&root_styles).unwrap();
        std::fs::write(root_styles.join("default.toml"), "name = 'default'\n").unwrap();

        let resolved = resolve_builtin_slides_styles_dir(&current_data).unwrap();

        assert_eq!(resolved, root_styles);
    }

    #[test]
    fn test_resolve_builtin_slides_styles_dir_does_not_use_unrelated_profile() {
        let dir = tempfile::TempDir::new().unwrap();
        let octos_home = dir.path().join(".octos");
        let current_data = octos_home
            .join("profiles")
            .join("dspfac--newsbot")
            .join("data");
        let unrelated_styles = octos_home
            .join("profiles")
            .join("someone-else")
            .join("data")
            .join("skills")
            .join("mofa-slides")
            .join("styles");

        std::fs::create_dir_all(&current_data).unwrap();
        std::fs::create_dir_all(&unrelated_styles).unwrap();
        std::fs::write(unrelated_styles.join("default.toml"), "name = 'default'\n").unwrap();

        let resolved = resolve_builtin_slides_styles_dir(&current_data);

        assert!(resolved.is_none());
    }

    #[test]
    fn session_task_query_store_hides_absolute_output_paths() {
        let dir = tempfile::TempDir::new().unwrap();
        let data_dir = dir.path().join("profile-data");
        let workspace = data_dir
            .join("users")
            .join("api%3Asession")
            .join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let output = workspace.join("voice.mp3");
        std::fs::write(&output, b"audio").unwrap();

        let supervisor = Arc::new(TaskSupervisor::new());
        let task_id = supervisor.register("fm_tts", "call-1", Some("api:session"));
        supervisor.mark_running(&task_id);
        supervisor.mark_runtime_state(
            &task_id,
            octos_agent::TaskRuntimeState::DeliveringOutputs,
            Some("send_file".to_string()),
        );
        supervisor.mark_completed(&task_id, vec![output.to_string_lossy().to_string()]);

        let store = SessionTaskQueryStore::default();
        let session_key = SessionKey::new("api", "session");
        store.register(&session_key, &supervisor, &data_dir);

        let payload = store.query_json(&session_key.to_string());
        let tasks = payload.as_array().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0]["runtime_state"], "completed");
        assert!(tasks[0]["runtime_detail"].is_null());
        let files = tasks[0]["output_files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        let handle = files[0].as_str().unwrap();
        assert!(handle.starts_with("pf/"));
        assert!(!handle.starts_with("/"));
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
                        provider_index: None,
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
            provider_index: None,
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
            attachment_media: vec![],
            attachment_prompt: None,
        }
    }

    fn make_attachment_inbound(summary: &str, attachment_path: &str) -> ActorMessage {
        ActorMessage::Inbound {
            message: InboundMessage {
                channel: "cli".to_string(),
                chat_id: "test".to_string(),
                sender_id: "user".to_string(),
                content: String::new(),
                timestamp: chrono::Utc::now(),
                media: vec![],
                metadata: serde_json::json!({}),
                message_id: None,
            },
            image_media: vec![],
            attachment_media: vec![attachment_path.to_string()],
            attachment_prompt: Some(summary.to_string()),
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
        let tools = octos_agent::ToolRegistry::with_builtins(dir.path());

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
            sender_user_id: None,
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
            overflow_cancelled: Arc::new(AtomicBool::new(false)),
            active_sessions: Arc::new(RwLock::new(ActiveSessionStore::open(dir.path()).unwrap())),
            user_workspace: dir.path().join("workspace"),
            cron_tool: None,
        };

        let handle = tokio::spawn(actor.run());
        (inbox_tx, out_rx, handle, session_mgr)
    }

    async fn setup_actor_with_timeout(
        agent_provider: Arc<dyn LlmProvider>,
        session_timeout: Duration,
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
        let tools = octos_agent::ToolRegistry::with_builtins(dir.path());

        let agent = Agent::new(AgentId::new("test-timeout"), agent_provider, tools, memory)
            .with_config(AgentConfig {
                save_episodes: false,
                max_iterations: 1,
                ..Default::default()
            });

        let (inbox_tx, inbox_rx) = mpsc::channel(32);
        let (out_tx, out_rx) = mpsc::channel(64);

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
            sender_user_id: None,
            user_status_config: UserStatusConfig::default(),
            data_dir: std::path::PathBuf::from("/tmp"),
            max_history: Arc::new(std::sync::atomic::AtomicUsize::new(50)),
            idle_timeout: Duration::from_secs(60),
            session_timeout,
            semaphore: Arc::new(Semaphore::new(10)),
            global_shutdown: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
            queue_mode: QueueMode::Followup,
            responsiveness: ResponsivenessObserver::new(),
            adaptive_router: None,
            memory_store: None,
            active_overflow_tasks: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            overflow_cancelled: Arc::new(AtomicBool::new(false)),
            active_sessions: Arc::new(RwLock::new(ActiveSessionStore::open(dir.path()).unwrap())),
            user_workspace: dir.path().join("workspace"),
            cron_tool: None,
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
        let tools = octos_agent::ToolRegistry::with_builtins(dir.path());

        let agent = Agent::new(AgentId::new("test-spec"), agent_provider, tools, memory)
            .with_config(AgentConfig {
                save_episodes: false,
                max_iterations: 1,
                ..Default::default()
            });

        // AdaptiveRouter with separate providers for overflow (serve_overflow only)
        let router = Arc::new(
            AdaptiveRouter::new(router_providers, &[], AdaptiveConfig::default())
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
            sender_user_id: None,
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
            overflow_cancelled: Arc::new(AtomicBool::new(false)),
            active_sessions: Arc::new(RwLock::new(ActiveSessionStore::open(dir.path()).unwrap())),
            user_workspace: dir.path().join("workspace"),
            cron_tool: None,
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
            kind: BackgroundResultKind::Report,
            media: vec![],
            ack: None,
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

    #[tokio::test]
    async fn test_followup_background_result_notifies_without_rewrite_turn() {
        let dir = tempfile::TempDir::new().unwrap();

        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![(Duration::from_secs(4), make_response("primary done"))],
        ));

        let (tx, mut rx, handle, _session_mgr) =
            setup_actor_with_mode(agent_llm, QueueMode::Followup, None, false, &dir).await;

        tx.send(make_inbound("long task")).await.unwrap();

        tokio::time::sleep(Duration::from_secs(1)).await;
        tx.send(ActorMessage::BackgroundResult {
            task_label: "research".to_string(),
            content: "Background research completed with 5 findings.".to_string(),
            kind: BackgroundResultKind::Report,
            media: vec![],
            ack: None,
        })
        .await
        .unwrap();

        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while responses.len() < 2 {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(msg)) => responses.push(msg.content),
                _ => break,
            }
        }

        assert!(
            responses
                .iter()
                .any(|r| r.contains("research") && r.contains("completed")),
            "background notification not found in: {:?}",
            responses
        );
        assert!(
            responses.iter().any(|r| r.contains("primary done")),
            "primary response not found in: {:?}",
            responses
        );

        let session_handle = SessionHandle::open(dir.path(), &SessionKey::new("cli", "test"));
        let session = session_handle.session();
        assert!(
            session
                .messages
                .iter()
                .any(|m| m.content.contains("Background task") && m.content.contains("research")),
            "background result not found in session history"
        );
        assert!(
            session
                .messages
                .iter()
                .all(|m| !m.content.contains("[REWRITE]")),
            "rewrite prompt leaked into session history: {:?}",
            session.messages
        );

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    #[tokio::test]
    async fn test_background_notification_persists_media_to_history() {
        let dir = tempfile::TempDir::new().unwrap();
        let media_path = dir.path().join("podcast_full_test.mp3");
        std::fs::write(&media_path, vec![1u8; 4096]).unwrap();

        let agent_llm = Arc::new(DelayedMockProvider::new("agent", vec![]));
        let (tx, mut rx, handle, _session_mgr) =
            setup_actor_with_mode(agent_llm, QueueMode::Followup, None, false, &dir).await;

        let (ack_tx, ack_rx) = oneshot::channel();
        tx.send(ActorMessage::BackgroundResult {
            task_label: "podcast_generate".to_string(),
            content: String::new(),
            kind: BackgroundResultKind::Notification,
            media: vec![media_path.to_string_lossy().to_string()],
            ack: Some(ack_tx),
        })
        .await
        .unwrap();

        assert!(
            tokio::time::timeout(Duration::from_secs(2), ack_rx)
                .await
                .expect("ack timeout")
                .expect("actor ack"),
            "background notification was not persisted"
        );

        let outbound = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("outbound timeout")
            .expect("outbound message");
        assert_eq!(
            outbound.media,
            vec![media_path.to_string_lossy().to_string()]
        );

        let session_handle = SessionHandle::open(dir.path(), &SessionKey::new("cli", "test"));
        let session = session_handle.session();
        let persisted = session.messages.iter().any(|message| {
            message.role == MessageRole::Assistant
                && message.media == vec![media_path.to_string_lossy().to_string()]
        });
        assert!(persisted, "media notification not found in session history");

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    #[tokio::test]
    async fn test_timeout_failure_persists_to_history() {
        let dir = tempfile::TempDir::new().unwrap();
        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![(Duration::from_millis(250), make_response("late reply"))],
        ));

        let (tx, mut rx, handle, _session_mgr) =
            setup_actor_with_timeout(agent_llm, Duration::from_millis(50), &dir).await;

        tx.send(make_inbound("slow request")).await.unwrap();

        let outbound = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timeout response")
            .expect("outbound timeout message");
        assert_eq!(outbound.content, "Processing timed out. Please try again.");

        let session_handle = SessionHandle::open(dir.path(), &SessionKey::new("cli", "test"));
        let session = session_handle.session();
        assert!(
            session
                .messages
                .iter()
                .any(|message| message.role == MessageRole::Assistant
                    && message.content == "Processing timed out. Please try again."),
            "timeout message not found in session history: {:?}",
            session
                .messages
                .iter()
                .map(|message| (message.role, message.content.clone()))
                .collect::<Vec<_>>()
        );

        drop(tx);
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }

    #[tokio::test]
    async fn test_attachment_hints_do_not_persist_in_session_history() {
        let dir = tempfile::TempDir::new().unwrap();

        let agent_llm = Arc::new(DelayedMockProvider::new(
            "agent",
            vec![(
                Duration::from_millis(50),
                make_response("attachment processed"),
            )],
        ));

        let (tx, mut rx, handle, _session_mgr) =
            setup_actor_with_mode(agent_llm, QueueMode::Followup, None, false, &dir).await;

        tx.send(make_attachment_inbound(
            "[Attached files]\n- report.pdf",
            "/tmp/uploads/report.pdf",
        ))
        .await
        .unwrap();

        let _ = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("response timeout")
            .expect("channel closed");

        let session_handle = SessionHandle::open(dir.path(), &SessionKey::new("cli", "test"));
        let session = session_handle.session();
        let contents = session
            .messages
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>();

        assert!(
            contents
                .iter()
                .any(|content| *content == "[User sent attachments]"),
            "generic attachment placeholder missing from history: {:?}",
            contents
        );
        assert!(
            contents
                .iter()
                .all(|content| !content.contains("[Attached files]")),
            "transient attachment prompt leaked into history: {:?}",
            contents
        );
        assert!(
            contents
                .iter()
                .all(|content| !content.contains("report.pdf")),
            "attachment filename leaked into history: {:?}",
            contents
        );

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

        // Send first message → goes through 500ms coalescing delay, then starts 2s processing
        tx.send(make_inbound("first message")).await.unwrap();

        // Wait for the 500ms coalescing + some processing time, then queue two more.
        // The first message must be past drain_queue before follow-ups arrive,
        // otherwise the coalescing delay will pick them up and steer immediately.
        tokio::time::sleep(Duration::from_millis(800)).await;
        tx.send(make_inbound("second message (discarded)"))
            .await
            .unwrap();
        tx.send(make_inbound("third message (newest)"))
            .await
            .unwrap();

        // Collect responses (expect 2: first + steered)
        let mut responses = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
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
            AdaptiveRouter::new(vec![router_a, router_b], &[], AdaptiveConfig::default())
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
            AdaptiveRouter::new(vec![router_a, router_b], &[], AdaptiveConfig::default())
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

    // ── Track B: dispatch profile routing tests ────────────────────────────

    /// Helper: create an ActorRegistry with a minimal ActorFactory for dispatch tests.
    async fn setup_dispatch_registry(
        dir: &tempfile::TempDir,
    ) -> (ActorRegistry, mpsc::Receiver<OutboundMessage>) {
        let provider: Arc<dyn LlmProvider> = Arc::new(DelayedMockProvider::new(
            "test",
            (0..20)
                .map(|_| (Duration::from_millis(100), make_response("ok")))
                .collect(),
        ));
        let memory = Arc::new(EpisodeStore::open(dir.path().join("memory")).await.unwrap());
        let session_mgr = Arc::new(Mutex::new(
            SessionManager::open(&dir.path().join("sessions")).unwrap(),
        ));
        let (out_tx, out_rx) = mpsc::channel(64);
        let tools = octos_agent::ToolRegistry::with_builtins(dir.path());
        let (spawn_tx, _spawn_rx) = mpsc::channel(32);

        let factory = ActorFactory {
            agent_config: AgentConfig {
                save_episodes: false,
                max_iterations: 1,
                ..Default::default()
            },
            llm: provider.clone(),
            llm_strong: provider.clone(),
            llm_for_compaction: provider.clone(),
            memory,
            system_prompt: Arc::new(std::sync::RwLock::new("default prompt".to_string())),
            hooks: None,
            hook_context_template: None,
            data_dir: dir.path().to_path_buf(),
            session_mgr,
            out_tx: out_tx.clone(),
            spawn_inbound_tx: spawn_tx,
            cron_service: None,
            tool_registry_factory: Arc::new(SnapshotToolRegistryFactory::new(tools)),
            pipeline_factory: None,
            max_history: Arc::new(std::sync::atomic::AtomicUsize::new(50)),
            idle_timeout: Duration::from_secs(60),
            session_timeout: Duration::from_secs(120),
            shutdown: Arc::new(AtomicBool::new(false)),
            cwd: dir.path().to_path_buf(),
            sandbox_config: octos_agent::SandboxConfig::default(),
            provider_policy: None,
            worker_prompt: None,
            provider_router: None,
            embedder: None,
            active_sessions: Arc::new(RwLock::new(ActiveSessionStore::open(dir.path()).unwrap())),
            pending_messages: Arc::new(Mutex::new(HashMap::new())),
            queue_mode: QueueMode::Followup,
            adaptive_router: None,
            memory_store: None,
            plugin_dirs: Vec::new(),
            plugin_extra_env: Vec::new(),
            task_query_store: SessionTaskQueryStore::default(),
        };

        let registry = ActorRegistry::new(
            factory,
            Arc::new(Semaphore::new(10)),
            out_tx,
            Arc::new(Mutex::new(HashMap::new())),
        );

        (registry, out_rx)
    }

    #[tokio::test]
    async fn test_dispatch_routes_by_profile_id() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut registry, _rx) = setup_dispatch_registry(&dir).await;

        let sk = SessionKey::new("matrix", "!room:localhost");
        let msg = InboundMessage {
            channel: "matrix".to_string(),
            sender_id: "user1".to_string(),
            chat_id: "!room:localhost".to_string(),
            content: "hello".to_string(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: serde_json::json!({}),
            message_id: None,
        };

        registry
            .dispatch(DispatchParams {
                message: msg,
                image_media: vec![],
                attachment_media: vec![],
                attachment_prompt: None,
                session_key: sk.clone(),
                reply_channel: "matrix",
                reply_chat_id: "!room:localhost",
                status_indicator: None,
                profile_id: Some("weather"),
                system_prompt_override: Some("You are a weather bot".to_string()),
                sender_user_id: Some("@octos_weather:localhost".to_string()),
            })
            .await;

        let keys = registry.actor_keys();
        assert_eq!(keys.len(), 1);
        assert!(
            keys[0].starts_with("weather:"),
            "dispatch key should start with profile_id, got: {}",
            keys[0]
        );
    }

    #[tokio::test]
    async fn test_dispatch_routes_to_default_profile() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut registry, _rx) = setup_dispatch_registry(&dir).await;

        let sk = SessionKey::new("matrix", "!room:localhost");
        let msg = InboundMessage {
            channel: "matrix".to_string(),
            sender_id: "user1".to_string(),
            chat_id: "!room:localhost".to_string(),
            content: "hello".to_string(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: serde_json::json!({}),
            message_id: None,
        };

        registry
            .dispatch(DispatchParams {
                message: msg,
                image_media: vec![],
                attachment_media: vec![],
                attachment_prompt: None,
                session_key: sk,
                reply_channel: "matrix",
                reply_chat_id: "!room:localhost",
                status_indicator: None,
                profile_id: None,
                system_prompt_override: None,
                sender_user_id: None,
            })
            .await;

        let keys = registry.actor_keys();
        assert_eq!(keys.len(), 1);
        assert!(
            keys[0].starts_with("_main:"),
            "dispatch key should start with _main when no profile_id, got: {}",
            keys[0]
        );
    }

    #[tokio::test]
    async fn test_dispatch_profile_and_main_create_separate_actors() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut registry, _rx) = setup_dispatch_registry(&dir).await;

        let sk = SessionKey::new("matrix", "!room:localhost");

        let msg1 = InboundMessage {
            channel: "matrix".to_string(),
            sender_id: "user1".to_string(),
            chat_id: "!room:localhost".to_string(),
            content: "hello weather".to_string(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: serde_json::json!({}),
            message_id: None,
        };
        registry
            .dispatch(DispatchParams {
                message: msg1,
                image_media: vec![],
                attachment_media: vec![],
                attachment_prompt: None,
                session_key: sk.clone(),
                reply_channel: "matrix",
                reply_chat_id: "!room:localhost",
                status_indicator: None,
                profile_id: Some("weather"),
                system_prompt_override: None,
                sender_user_id: None,
            })
            .await;

        let msg2 = InboundMessage {
            channel: "matrix".to_string(),
            sender_id: "user1".to_string(),
            chat_id: "!room:localhost".to_string(),
            content: "hello main".to_string(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: serde_json::json!({}),
            message_id: None,
        };
        registry
            .dispatch(DispatchParams {
                message: msg2,
                image_media: vec![],
                attachment_media: vec![],
                attachment_prompt: None,
                session_key: sk,
                reply_channel: "matrix",
                reply_chat_id: "!room:localhost",
                status_indicator: None,
                profile_id: None,
                system_prompt_override: None,
                sender_user_id: None,
            })
            .await;

        let keys = registry.actor_keys();
        assert_eq!(
            keys.len(),
            2,
            "different profile_ids should create separate actors, got keys: {:?}",
            keys
        );
        assert!(
            keys.iter().any(|k| k.starts_with("weather:")),
            "should have weather-prefixed actor"
        );
        assert!(
            keys.iter().any(|k| k.starts_with("_main:")),
            "should have _main-prefixed actor"
        );
    }

    #[tokio::test]
    async fn test_cancel_matches_profile_scoped_actor_by_session_key() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut registry, _rx) = setup_dispatch_registry(&dir).await;

        let sk = SessionKey::new("matrix", "!room:localhost");
        let msg = InboundMessage {
            channel: "matrix".to_string(),
            sender_id: "user1".to_string(),
            chat_id: "!room:localhost".to_string(),
            content: "hello weather".to_string(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: serde_json::json!({}),
            message_id: None,
        };
        registry
            .dispatch(DispatchParams {
                message: msg,
                image_media: vec![],
                attachment_media: vec![],
                attachment_prompt: None,
                session_key: sk.clone(),
                reply_channel: "matrix",
                reply_chat_id: "!room:localhost",
                status_indicator: None,
                profile_id: Some("weather"),
                system_prompt_override: None,
                sender_user_id: Some("@octos_weather:localhost".to_string()),
            })
            .await;

        registry.cancel(&sk.to_string()).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        registry.reap_dead_actors();

        assert!(
            registry.actor_keys().is_empty(),
            "cancel should stop the profiled actor when called with the bare session key"
        );
    }

    #[test]
    fn test_sender_metadata_for_system_notice_includes_virtual_user() {
        let metadata = system_notice_metadata(Some("@octos_weather:localhost"));

        assert_eq!(
            metadata
                .get(METADATA_SENDER_USER_ID)
                .and_then(|v| v.as_str()),
            Some("@octos_weather:localhost")
        );
    }

    #[tokio::test]
    async fn test_profile_session_keys_are_persisted_separately() {
        let dir = tempfile::TempDir::new().unwrap();
        let weather_key = SessionKey::with_profile("weather", "matrix", "!room:localhost");
        let news_key = SessionKey::with_profile("news", "matrix", "!room:localhost");

        let mut weather = SessionHandle::open(dir.path(), &weather_key);
        weather
            .add_message(Message::user("weather message"))
            .await
            .unwrap();

        let mut news = SessionHandle::open(dir.path(), &news_key);
        news.add_message(Message::user("news message"))
            .await
            .unwrap();

        let weather = SessionHandle::open(dir.path(), &weather_key);
        let news = SessionHandle::open(dir.path(), &news_key);

        assert_eq!(weather.get_history(10).len(), 1);
        assert_eq!(news.get_history(10).len(), 1);
        assert_eq!(weather.get_history(10)[0].content, "weather message");
        assert_eq!(news.get_history(10)[0].content, "news message");
    }

    #[test]
    fn forced_background_workflow_detects_deep_research() {
        assert_eq!(
            ForcedBackgroundWorkflow::detect(
                "请对「全球AI代理竞争格局」做一次深度研究，并输出完整报告。"
            ),
            Some(ForcedBackgroundWorkflow::DeepResearch)
        );
    }

    #[test]
    fn forced_background_workflow_detects_research_podcast() {
        assert_eq!(
            ForcedBackgroundWorkflow::detect(
                "用杨幂和窦文涛的声音做一个播客，播报一下北京今日的热点新闻，要求专业冷静。"
            ),
            Some(ForcedBackgroundWorkflow::ResearchPodcast)
        );
    }

    #[test]
    fn forced_background_workflow_respects_foreground_override() {
        assert_eq!(
            ForcedBackgroundWorkflow::detect(
                "请同步等待完成，不要后台。对这个主题做深度研究并直接在这里输出。"
            ),
            None
        );
    }
}
