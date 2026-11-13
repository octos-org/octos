//! Gateway command dispatcher: testable extraction of the gateway main loop's
//! slash-command routing and callback-query handling.
//!
//! Each method corresponds to a branch in the old monolithic `while let` loop.
//! The caller (`run_async`) feeds inbound messages and acts on [`DispatchResult`].

use std::path::PathBuf;
use std::sync::Arc;

use octos_bus::{ActiveSessionStore, SessionManager, validate_topic_name};
use octos_core::{InboundMessage, MAIN_PROFILE_ID, OutboundMessage, SessionKey};
use tokio::sync::{Mutex, RwLock, mpsc};
use tracing::{info, warn};

use crate::commands::gateway::{build_profiled_session_key, session_ui};
use crate::session_actor::PendingMessages;

// ── Result of dispatching a single inbound message ──────────────────────────

/// What the main loop should do after the dispatcher processes a message.
#[derive(Debug)]
pub enum DispatchResult {
    /// Command was handled; loop should `continue` (skip actor dispatch).
    Handled,
    /// Not a command — forward to the session actor for LLM processing.
    Forward,
}

// ── Dispatcher ──────────────────────────────────────────────────────────────

/// Extracted command dispatcher for the gateway main loop.
///
/// Holds shared references to session state, pending messages buffer,
/// and the outbound channel. Fully testable without LLM or actor dependencies.
pub struct GatewayDispatcher {
    pub(crate) session_mgr: Arc<Mutex<SessionManager>>,
    pub(crate) active_sessions: Arc<RwLock<ActiveSessionStore>>,
    pub(crate) pending_messages: PendingMessages,
    pub(crate) out_tx: mpsc::Sender<OutboundMessage>,
    pub(crate) session_delete_tx: Option<mpsc::UnboundedSender<String>>,
    /// Profile ID used for building profiled session keys (None = main profile).
    pub(crate) dispatch_profile_id: Option<String>,
    /// Profile data directory for per-user files (soul.md, etc.).
    pub(crate) data_dir: Option<PathBuf>,
}

impl GatewayDispatcher {
    pub fn new(
        session_mgr: Arc<Mutex<SessionManager>>,
        active_sessions: Arc<RwLock<ActiveSessionStore>>,
        pending_messages: PendingMessages,
        out_tx: mpsc::Sender<OutboundMessage>,
    ) -> Self {
        Self {
            session_mgr,
            active_sessions,
            pending_messages,
            out_tx,
            session_delete_tx: None,
            dispatch_profile_id: None,
            data_dir: None,
        }
    }

    /// Set the dispatch profile ID for profiled session key construction.
    pub fn with_profile_id(mut self, profile_id: Option<String>) -> Self {
        self.dispatch_profile_id = profile_id;
        self
    }

    /// Set the data directory for per-user files (soul.md, etc.).
    pub fn with_data_dir(mut self, data_dir: PathBuf) -> Self {
        self.data_dir = Some(data_dir);
        self
    }

    /// Set the actor-removal event channel used when a session is deleted.
    pub fn with_session_delete_tx(mut self, tx: mpsc::UnboundedSender<String>) -> Self {
        self.session_delete_tx = Some(tx);
        self
    }

    /// Build a profiled session key for the given channel/chat/topic.
    fn profiled_key(&self, channel: &str, chat_id: &str, topic: &str) -> SessionKey {
        build_profiled_session_key(self.dispatch_profile_id.as_deref(), channel, chat_id, topic)
    }

    fn notify_session_deleted(&self, session_key: &SessionKey) {
        if let Some(tx) = &self.session_delete_tx {
            let _ = tx.send(session_key.to_string());
        }
    }

    /// Flush pending (buffered) messages for a session key, delivering them
    /// through `out_tx`. Returns the number of messages flushed.
    async fn flush_pending(&self, session_key: &str) -> usize {
        let messages = self.pending_messages.take(session_key);
        let count = messages.len();
        for msg in messages {
            let _ = self.out_tx.send(msg).await;
        }
        count
    }

    // ── Session commands ────────────────────────────────────────────────

    /// Handle `/new` or `/new <name>`.
    pub async fn handle_new_command(
        &self,
        cmd: &str,
        session_key: &SessionKey,
        reply_channel: &str,
        reply_chat_id: &str,
        base_key_str: &str,
    ) -> Option<DispatchResult> {
        if cmd != "/new" && cmd != "/clear" && !cmd.starts_with("/new ") {
            return None;
        }
        let name = if cmd == "/clear" {
            ""
        } else {
            cmd.strip_prefix("/new").unwrap_or("").trim()
        };
        if name.is_empty() {
            match self.session_mgr.lock().await.clear(session_key).await {
                Ok(()) => {
                    let _ = self
                        .out_tx
                        .send(make_reply(reply_channel, reply_chat_id, "Session cleared."))
                        .await;
                }
                Err(e) => {
                    warn!("session clear failed: {e}");
                }
            }
        } else if let Err(reason) = validate_topic_name(name) {
            let _ = self
                .out_tx
                .send(make_reply(
                    reply_channel,
                    reply_chat_id,
                    format!("Invalid session name: {reason}"),
                ))
                .await;
        } else {
            // Issue #1016: normalize the plural `sites` alias to the canonical
            // singular `site` form BEFORE touching session state. The WS path
            // (`ws_slash::handle_new`) does the same — power users and parallel
            // frontends typing `/new sites astro` by analogy with `/new slides
            // …` (which IS plural) would otherwise fall through to the generic
            // switch-session arm and never trigger the site scaffold. Aliasing
            // here lets one branch handle both forms.
            //
            // Codex round-1 fixup: normalize BEFORE `switch_to` /
            // `touch_user_session` / `try_activate_*_template`. Otherwise the
            // active gateway topic + session file land on `sites astro` while
            // the scaffolded prompt + workspace land on `site astro`, and the
            // gateway runtime's prompt lookup (keyed by active topic) misses
            // entirely.
            let normalized_name: std::borrow::Cow<'_, str> = if name == "sites" {
                std::borrow::Cow::Borrowed("site")
            } else if let Some(rest) = name.strip_prefix("sites ") {
                std::borrow::Cow::Owned(format!("site {rest}"))
            } else {
                std::borrow::Cow::Borrowed(name)
            };
            let name: &str = normalized_name.as_ref();

            self.active_sessions
                .write()
                .await
                .switch_to(base_key_str, name)
                .unwrap_or_else(|e| warn!("switch_to failed: {e}"));

            // Ensure the session file exists on disk so /sessions can list it.
            self.session_mgr
                .lock()
                .await
                .touch_user_session(base_key_str, name);

            // Check for project templates (e.g. "/new slides <project>")
            let reply = if name == "slides" || name.starts_with("slides ") {
                if let Some(data_dir) = &self.data_dir {
                    let encoded_base =
                        octos_bus::session::encode_path_component(session_key.base_key());
                    let workspace_root =
                        data_dir.join("users").join(encoded_base).join("workspace");
                    std::fs::create_dir_all(&workspace_root).ok();

                    match crate::project_templates::try_activate_slides_template(data_dir, name) {
                        Some(template_reply) => {
                            let project_name = name.strip_prefix("slides").unwrap_or("").trim();
                            let project_name = if project_name.is_empty() {
                                "untitled"
                            } else {
                                project_name
                            };
                            match crate::project_templates::scaffold_slides_project(
                                &workspace_root,
                                project_name,
                            ) {
                                Ok(_) => template_reply,
                                Err(error) => {
                                    warn!(topic = name, "slides scaffold failed: {error}");
                                    format!(
                                        "{template_reply}\n\nSlides git/bootstrap failed: {error}"
                                    )
                                }
                            }
                        }
                        None => format!("Switched to session: {name}"),
                    }
                } else {
                    format!("Switched to session: {name}")
                }
            } else if name == "site" || name.starts_with("site ") {
                if let Some(data_dir) = &self.data_dir {
                    let _ = crate::project_templates::try_activate_site_template(data_dir, name);
                    let profile_id = self
                        .dispatch_profile_id
                        .clone()
                        .unwrap_or_else(|| MAIN_PROFILE_ID.to_string());
                    let encoded_base =
                        octos_bus::session::encode_path_component(session_key.base_key());
                    let workspace_root =
                        data_dir.join("users").join(encoded_base).join("workspace");
                    std::fs::create_dir_all(&workspace_root).ok();
                    match crate::project_templates::scaffold_site_project(
                        &workspace_root,
                        &profile_id,
                        crate::project_templates::preview_session_id(session_key),
                        name,
                        data_dir,
                    ) {
                        Ok(metadata) => crate::project_templates::site_creation_reply(&metadata),
                        Err(error) => {
                            warn!(topic = name, "site scaffold failed: {error}");
                            format!("Switched to session: {name}\n\nSite scaffold failed: {error}")
                        }
                    }
                } else {
                    format!("Switched to session: {name}")
                }
            } else {
                format!("Switched to session: {name}")
            };

            let _ = self
                .out_tx
                .send(make_reply(reply_channel, reply_chat_id, reply))
                .await;
        }
        Some(DispatchResult::Handled)
    }

    /// Handle `/s` or `/s <name>`.
    pub async fn handle_s_command(
        &self,
        cmd: &str,
        inbound: &InboundMessage,
        reply_channel: &str,
        reply_chat_id: &str,
        base_key_str: &str,
    ) -> Option<DispatchResult> {
        if cmd != "/s"
            && cmd != "/switch"
            && !cmd.starts_with("/s ")
            && !cmd.starts_with("/switch ")
        {
            return None;
        }
        let name = cmd
            .strip_prefix("/switch")
            .or_else(|| cmd.strip_prefix("/s"))
            .unwrap_or("")
            .trim();
        if name.is_empty() {
            self.active_sessions
                .write()
                .await
                .switch_to(base_key_str, "")
                .unwrap_or_else(|e| warn!("switch_to failed: {e}"));
            let _ = self
                .out_tx
                .send(make_reply(
                    reply_channel,
                    reply_chat_id,
                    "Switched to default session.",
                ))
                .await;
            let target_key = self.profiled_key(&inbound.channel, &inbound.chat_id, "");
            self.flush_pending(&target_key.to_string()).await;
        } else if let Err(reason) = validate_topic_name(name) {
            let _ = self
                .out_tx
                .send(make_reply(
                    reply_channel,
                    reply_chat_id,
                    format!("Invalid session name: {reason}"),
                ))
                .await;
        } else {
            self.active_sessions
                .write()
                .await
                .switch_to(base_key_str, name)
                .unwrap_or_else(|e| warn!("switch_to failed: {e}"));

            // Show last 2 messages as context preview
            let new_key = self.profiled_key(&inbound.channel, &inbound.chat_id, name);
            let preview = {
                let mut mgr = self.session_mgr.lock().await;
                let session = mgr.get_or_create(&new_key).await;
                let history = session.get_history(2);
                if history.is_empty() {
                    String::new()
                } else {
                    let mut lines = String::from("\n---\n");
                    for m in history {
                        let role = m.role.as_str();
                        let text: String = m.content.chars().take(100).collect();
                        lines.push_str(&format!("[{role}] {text}\n"));
                    }
                    lines
                }
            };

            let _ = self
                .out_tx
                .send(make_reply(
                    reply_channel,
                    reply_chat_id,
                    format!("Switched to session: {name}{preview}"),
                ))
                .await;

            self.flush_pending(&new_key.to_string()).await;
        }
        Some(DispatchResult::Handled)
    }

    /// Handle `/sessions` command.
    pub async fn handle_sessions_command(
        &self,
        cmd: &str,
        reply_channel: &str,
        reply_chat_id: &str,
        base_key_str: &str,
    ) -> Option<DispatchResult> {
        if cmd != "/sessions" {
            return None;
        }
        let entries = self
            .session_mgr
            .lock()
            .await
            .list_user_sessions(base_key_str);
        let active_topic = self
            .active_sessions
            .read()
            .await
            .get_active_topic(base_key_str)
            .to_string();

        if entries.is_empty() {
            let _ = self
                .out_tx
                .send(make_reply(
                    reply_channel,
                    reply_chat_id,
                    "No sessions found. Use /new <name> to create one.",
                ))
                .await;
        } else {
            let keyboard = session_ui::build_session_keyboard(&entries, &active_topic);
            let text = session_ui::build_session_text(&entries, &active_topic);
            let mut msg = make_reply(reply_channel, reply_chat_id, text);
            msg.metadata = keyboard;
            let _ = self.out_tx.send(msg).await;
        }
        Some(DispatchResult::Handled)
    }

    /// Handle `/back` or `/b` command.
    pub async fn handle_back_command(
        &self,
        cmd: &str,
        inbound: &InboundMessage,
        reply_channel: &str,
        reply_chat_id: &str,
        base_key_str: &str,
    ) -> Option<DispatchResult> {
        if cmd != "/back" && cmd != "/b" {
            return None;
        }
        let result = self.active_sessions.write().await.go_back(base_key_str);
        match result {
            Ok(Some(topic)) => {
                let label = if topic.is_empty() {
                    "(default)".to_string()
                } else {
                    topic.clone()
                };
                let _ = self
                    .out_tx
                    .send(make_reply(
                        reply_channel,
                        reply_chat_id,
                        format!("Switched back to session: {label}"),
                    ))
                    .await;

                let target_key = self.profiled_key(&inbound.channel, &inbound.chat_id, &topic);
                self.flush_pending(&target_key.to_string()).await;
            }
            Ok(None) => {
                let _ = self
                    .out_tx
                    .send(make_reply(
                        reply_channel,
                        reply_chat_id,
                        "No previous session to switch to.",
                    ))
                    .await;
            }
            Err(e) => {
                warn!("go_back failed: {e}");
            }
        }
        Some(DispatchResult::Handled)
    }

    /// Handle `/delete <name>` command.
    pub async fn handle_delete_command(
        &self,
        cmd: &str,
        inbound: &InboundMessage,
        reply_channel: &str,
        reply_chat_id: &str,
        base_key_str: &str,
    ) -> Option<DispatchResult> {
        if cmd != "/delete"
            && cmd != "/d"
            && !cmd.starts_with("/delete ")
            && !cmd.starts_with("/d ")
        {
            return None;
        }
        let name = cmd
            .strip_prefix("/delete")
            .or_else(|| cmd.strip_prefix("/d"))
            .unwrap_or("")
            .trim();
        if name.is_empty() {
            let current_topic = self
                .active_sessions
                .read()
                .await
                .get_active_topic(base_key_str)
                .to_string();
            if current_topic.is_empty() {
                let _ = self
                    .out_tx
                    .send(make_reply(
                        reply_channel,
                        reply_chat_id,
                        "Cannot delete the default session. Use /clear to reset it.",
                    ))
                    .await;
            } else {
                let del_key = self.profiled_key(&inbound.channel, &inbound.chat_id, &current_topic);
                match self.session_mgr.lock().await.delete(&del_key).await {
                    Ok(()) => {
                        self.notify_session_deleted(&del_key);
                        self.active_sessions
                            .write()
                            .await
                            .remove_topic(base_key_str, &current_topic)
                            .unwrap_or_else(|e| warn!("remove_topic failed: {e}"));
                        let _ = self
                            .out_tx
                            .send(make_reply(
                                reply_channel,
                                reply_chat_id,
                                format!("Deleted session: {current_topic}"),
                            ))
                            .await;
                    }
                    Err(e) => {
                        warn!("session delete failed: {e}");
                    }
                }
            }
        } else {
            let del_key = self.profiled_key(&inbound.channel, &inbound.chat_id, name);
            match self.session_mgr.lock().await.delete(&del_key).await {
                Ok(()) => {
                    self.notify_session_deleted(&del_key);
                    self.active_sessions
                        .write()
                        .await
                        .remove_topic(base_key_str, name)
                        .unwrap_or_else(|e| warn!("remove_topic failed: {e}"));
                    let _ = self
                        .out_tx
                        .send(make_reply(
                            reply_channel,
                            reply_chat_id,
                            format!("Deleted session: {name}"),
                        ))
                        .await;
                }
                Err(e) => {
                    warn!("delete session failed: {e}");
                }
            }
        }
        Some(DispatchResult::Handled)
    }

    /// Handle inline keyboard callback queries for session switching.
    /// Returns `Some(Handled)` if the callback was a session switch (`s:topic`).
    /// Returns `None` if the callback data doesn't match, so the caller
    /// should fall through to normal message processing.
    #[allow(clippy::too_many_arguments)]
    pub async fn handle_session_callback(
        &self,
        callback_data: &str,
        callback_message_id: Option<&str>,
        inbound: &InboundMessage,
        reply_channel: &str,
        reply_chat_id: &str,
        base_key_str: &str,
        channel_mgr: Option<&octos_bus::ChannelManager>,
    ) -> Option<DispatchResult> {
        let topic = callback_data.strip_prefix("s:")?;

        self.active_sessions
            .write()
            .await
            .switch_to(base_key_str, topic)
            .unwrap_or_else(|e| warn!("switch_to failed: {e}"));

        // Rebuild keyboard with updated active marker
        let entries = self
            .session_mgr
            .lock()
            .await
            .list_user_sessions(base_key_str);
        let keyboard = session_ui::build_session_keyboard(&entries, topic);
        let text = session_ui::build_session_text(&entries, topic);

        // Edit the picker message in-place
        if let Some(mid) = callback_message_id {
            if let Some(mgr) = channel_mgr {
                if let Some(ch) = mgr.get_channel(reply_channel) {
                    if let Err(e) = ch
                        .edit_message_with_metadata(reply_chat_id, mid, &text, &keyboard)
                        .await
                    {
                        warn!("failed to edit session picker: {e}");
                    }
                }
            }
        }

        let label = if topic.is_empty() { "(default)" } else { topic };
        info!(session = %label, "session switched via inline keyboard");

        let target_key = self.profiled_key(&inbound.channel, &inbound.chat_id, topic);
        self.flush_pending(&target_key.to_string()).await;

        Some(DispatchResult::Handled)
    }

    /// Handle `/soul`, `/soul show`, `/soul reset`, `/soul <text>`.
    pub async fn handle_soul_command(
        &self,
        cmd: &str,
        inbound: &InboundMessage,
        reply_channel: &str,
        reply_chat_id: &str,
    ) -> Option<DispatchResult> {
        if cmd != "/soul" && !cmd.starts_with("/soul ") {
            return None;
        }
        let data_dir = match &self.data_dir {
            Some(d) => d,
            None => {
                let _ = self
                    .out_tx
                    .send(make_reply(
                        reply_channel,
                        reply_chat_id,
                        "Soul not available (no data directory configured).",
                    ))
                    .await;
                return Some(DispatchResult::Handled);
            }
        };

        let arg = cmd.strip_prefix("/soul").unwrap_or("").trim();
        let soul_key = self.profiled_key(&inbound.channel, &inbound.chat_id, "");

        if arg.is_empty() || arg.eq_ignore_ascii_case("show") {
            // Report the EFFECTIVE soul with its source: a per-chat override
            // wins, otherwise the profile-wide soul (dashboard `/api/my/soul`)
            // still reaches actors and must not be masked as "No custom soul".
            let reply = match crate::soul_service::read_soul_for_session(data_dir, &soul_key) {
                Some(content) => format!("🪶 Current soul (this chat):\n\n{content}"),
                None => match crate::soul_service::read_soul(data_dir) {
                    Some(content) => {
                        format!("🪶 Current soul (profile-wide, no chat override):\n\n{content}")
                    }
                    None => "No custom soul set. Using default.".to_string(),
                },
            };
            let _ = self
                .out_tx
                .send(make_reply(reply_channel, reply_chat_id, reply))
                .await;
        } else if arg.eq_ignore_ascii_case("reset") {
            match crate::soul_service::remove_soul_for_session(data_dir, &soul_key) {
                Ok(()) => {
                    // Only the per-chat override is removed; if a profile-wide
                    // soul exists it remains effective — say so.
                    let msg = if crate::soul_service::read_soul(data_dir).is_some() {
                        "Chat soul override removed; the profile-wide soul still applies. \
                         Takes effect in new sessions."
                    } else {
                        "Soul reset to default. Takes effect in new sessions."
                    };
                    let _ = self
                        .out_tx
                        .send(make_reply(reply_channel, reply_chat_id, msg))
                        .await;
                }
                Err(e) => {
                    warn!("failed to remove soul: {e}");
                    let _ = self
                        .out_tx
                        .send(make_reply(
                            reply_channel,
                            reply_chat_id,
                            format!("Failed to reset soul: {e}"),
                        ))
                        .await;
                }
            }
        } else {
            match crate::soul_service::write_soul_for_session(data_dir, &soul_key, arg) {
                Ok(()) => {
                    info!(session = %soul_key, soul_len = arg.len(), "user soul updated");
                    let _ = self
                        .out_tx
                        .send(make_reply(
                            reply_channel,
                            reply_chat_id,
                            "Soul updated. Takes effect in new sessions.",
                        ))
                        .await;
                }
                Err(e) => {
                    warn!("failed to write soul: {e}");
                    let _ = self
                        .out_tx
                        .send(make_reply(
                            reply_channel,
                            reply_chat_id,
                            format!("Failed to update soul: {e}"),
                        ))
                        .await;
                }
            }
        }

        Some(DispatchResult::Handled)
    }

    /// Try all session commands in order. Returns `Handled` if matched,
    /// `Forward` if the message should go to the actor for LLM processing.
    pub async fn try_dispatch_session_command(
        &self,
        cmd: &str,
        inbound: &InboundMessage,
        session_key: &SessionKey,
        reply_channel: &str,
        reply_chat_id: &str,
        base_key_str: &str,
    ) -> DispatchResult {
        if let Some(r) = self
            .handle_new_command(cmd, session_key, reply_channel, reply_chat_id, base_key_str)
            .await
        {
            return r;
        }
        if let Some(r) = self
            .handle_s_command(cmd, inbound, reply_channel, reply_chat_id, base_key_str)
            .await
        {
            return r;
        }
        if let Some(r) = self
            .handle_sessions_command(cmd, reply_channel, reply_chat_id, base_key_str)
            .await
        {
            return r;
        }
        if let Some(r) = self
            .handle_back_command(cmd, inbound, reply_channel, reply_chat_id, base_key_str)
            .await
        {
            return r;
        }
        if let Some(r) = self
            .handle_delete_command(cmd, inbound, reply_channel, reply_chat_id, base_key_str)
            .await
        {
            return r;
        }
        if let Some(r) = self
            .handle_soul_command(cmd, inbound, reply_channel, reply_chat_id)
            .await
        {
            return r;
        }
        DispatchResult::Forward
    }
}

/// Build a simple text reply to send back on the same channel/chat.
pub(crate) fn make_reply(
    channel: &str,
    chat_id: &str,
    content: impl Into<String>,
) -> OutboundMessage {
    OutboundMessage {
        channel: channel.to_string(),
        chat_id: chat_id.to_string(),
        content: content.into(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({}),
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────
