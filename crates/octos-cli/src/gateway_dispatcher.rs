//! Gateway command dispatcher: testable extraction of the gateway main loop's
//! slash-command routing and callback-query handling.
//!
//! Each method corresponds to a branch in the old monolithic `while let` loop.
//! The caller (`run_async`) feeds inbound messages and acts on [`DispatchResult`].

use std::sync::Arc;

use octos_bus::{ActiveSessionStore, SessionManager, validate_topic_name};
use octos_core::{InboundMessage, OutboundMessage, SessionKey};
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
    /// Profile ID used for building profiled session keys (None = main profile).
    pub(crate) dispatch_profile_id: Option<String>,
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
            dispatch_profile_id: None,
        }
    }

    /// Set the dispatch profile ID for profiled session key construction.
    pub fn with_profile_id(mut self, profile_id: Option<String>) -> Self {
        self.dispatch_profile_id = profile_id;
        self
    }

    /// Build a profiled session key for the given channel/chat/topic.
    fn profiled_key(&self, channel: &str, chat_id: &str, topic: &str) -> SessionKey {
        build_profiled_session_key(self.dispatch_profile_id.as_deref(), channel, chat_id, topic)
    }

    /// Flush pending (buffered) messages for a session key, delivering them
    /// through `out_tx`. Returns the number of messages flushed.
    async fn flush_pending(&self, session_key: &str) -> usize {
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
        if cmd != "/new" && !cmd.starts_with("/new ") {
            return None;
        }
        let name = cmd.strip_prefix("/new").unwrap_or("").trim();
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

            let _ = self
                .out_tx
                .send(make_reply(
                    reply_channel,
                    reply_chat_id,
                    format!("Switched to session: {name}"),
                ))
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
        if cmd != "/s" && !cmd.starts_with("/s ") {
            return None;
        }
        let name = cmd.strip_prefix("/s").unwrap_or("").trim();
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
                let session = mgr.get_or_create(&new_key);
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
        if !cmd.starts_with("/delete ") {
            return None;
        }
        let name = cmd.strip_prefix("/delete").unwrap_or("").trim();
        if name.is_empty() {
            let _ = self
                .out_tx
                .send(make_reply(
                    reply_channel,
                    reply_chat_id,
                    "Usage: /delete <session-name>",
                ))
                .await;
        } else {
            let del_key = self.profiled_key(&inbound.channel, &inbound.chat_id, name);
            match self.session_mgr.lock().await.clear(&del_key).await {
                Ok(()) => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::sync::mpsc;

    use crate::commands::gateway::build_profiled_session_key;
    use crate::session_actor::PendingMessages;

    /// Create a test dispatcher with fresh temp dirs and shared pending buffer.
    fn setup_dispatcher(
        out_tx: mpsc::Sender<OutboundMessage>,
    ) -> (GatewayDispatcher, PendingMessages, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();

        let session_mgr = Arc::new(Mutex::new(SessionManager::open(tmp.path()).unwrap()));
        let active_sessions = Arc::new(RwLock::new(ActiveSessionStore::open(tmp.path()).unwrap()));
        let pending: PendingMessages = Arc::new(Mutex::new(HashMap::new()));
        let dispatcher =
            GatewayDispatcher::new(session_mgr, active_sessions, pending.clone(), out_tx);
        (dispatcher, pending, tmp)
    }

    fn make_test_inbound(channel: &str, chat_id: &str, content: &str) -> InboundMessage {
        InboundMessage {
            channel: channel.to_string(),
            chat_id: chat_id.to_string(),
            sender_id: "user1".to_string(),
            content: content.to_string(),
            timestamp: chrono::Utc::now(),
            media: vec![],
            metadata: serde_json::json!({}),
            message_id: None,
        }
    }

    // ── /new tests ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn should_clear_session_when_new_without_name() {
        let (tx, mut rx) = mpsc::channel(16);
        let (disp, _, _tmp) = setup_dispatcher(tx);
        let session_key = SessionKey::new("telegram", "123");

        let result = disp
            .handle_new_command("/new", &session_key, "telegram", "123", "telegram:123")
            .await;

        assert!(matches!(result, Some(DispatchResult::Handled)));
        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.content, "Session cleared.");
    }

    #[tokio::test]
    async fn should_create_named_session_when_new_with_name() {
        let (tx, mut rx) = mpsc::channel(16);
        let (disp, _, _tmp) = setup_dispatcher(tx);
        let session_key = SessionKey::new("telegram", "123");

        let result = disp
            .handle_new_command(
                "/new research",
                &session_key,
                "telegram",
                "123",
                "telegram:123",
            )
            .await;

        assert!(matches!(result, Some(DispatchResult::Handled)));
        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.content, "Switched to session: research");

        let topic = disp
            .active_sessions
            .read()
            .await
            .get_active_topic("telegram:123")
            .to_string();
        assert_eq!(topic, "research");
    }

    #[tokio::test]
    async fn should_reject_invalid_session_name_on_new() {
        let (tx, mut rx) = mpsc::channel(16);
        let (disp, _, _tmp) = setup_dispatcher(tx);
        let session_key = SessionKey::new("telegram", "123");

        let long_name = "a".repeat(51);
        let cmd = format!("/new {long_name}");
        let result = disp
            .handle_new_command(&cmd, &session_key, "telegram", "123", "telegram:123")
            .await;

        assert!(matches!(result, Some(DispatchResult::Handled)));
        let msg = rx.try_recv().unwrap();
        assert!(msg.content.starts_with("Invalid session name:"));
    }

    #[tokio::test]
    async fn should_return_none_when_not_new_command() {
        let (tx, _rx) = mpsc::channel(16);
        let (disp, _, _tmp) = setup_dispatcher(tx);
        let session_key = SessionKey::new("telegram", "123");

        let result = disp
            .handle_new_command("/sessions", &session_key, "telegram", "123", "telegram:123")
            .await;

        assert!(result.is_none());
    }

    // ── /s tests ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn should_switch_to_default_session_on_bare_s() {
        let (tx, mut rx) = mpsc::channel(16);
        let (disp, _, _tmp) = setup_dispatcher(tx);
        let inbound = make_test_inbound("telegram", "123", "/s");

        // First switch to a topic
        disp.active_sessions
            .write()
            .await
            .switch_to("telegram:123", "research")
            .unwrap();

        let result = disp
            .handle_s_command("/s", &inbound, "telegram", "123", "telegram:123")
            .await;

        assert!(matches!(result, Some(DispatchResult::Handled)));
        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.content, "Switched to default session.");

        let topic = disp
            .active_sessions
            .read()
            .await
            .get_active_topic("telegram:123")
            .to_string();
        assert_eq!(topic, "");
    }

    #[tokio::test]
    async fn should_switch_to_named_session_on_s_name() {
        let (tx, mut rx) = mpsc::channel(16);
        let (disp, _, _tmp) = setup_dispatcher(tx);
        let inbound = make_test_inbound("telegram", "123", "/s coding");

        let result = disp
            .handle_s_command("/s coding", &inbound, "telegram", "123", "telegram:123")
            .await;

        assert!(matches!(result, Some(DispatchResult::Handled)));
        let msg = rx.try_recv().unwrap();
        assert!(msg.content.starts_with("Switched to session: coding"));

        let topic = disp
            .active_sessions
            .read()
            .await
            .get_active_topic("telegram:123")
            .to_string();
        assert_eq!(topic, "coding");
    }

    #[tokio::test]
    async fn should_reject_invalid_name_on_s_command() {
        let (tx, mut rx) = mpsc::channel(16);
        let (disp, _, _tmp) = setup_dispatcher(tx);
        let inbound = make_test_inbound("telegram", "123", "/s ");

        let long = "x".repeat(51);
        let cmd = format!("/s {long}");
        let result = disp
            .handle_s_command(&cmd, &inbound, "telegram", "123", "telegram:123")
            .await;

        assert!(matches!(result, Some(DispatchResult::Handled)));
        let msg = rx.try_recv().unwrap();
        assert!(msg.content.starts_with("Invalid session name:"));
    }

    // ── /sessions tests ─────────────────────────────────────────────────

    #[tokio::test]
    async fn should_list_empty_sessions() {
        let (tx, mut rx) = mpsc::channel(16);
        let (disp, _, _tmp) = setup_dispatcher(tx);

        let result = disp
            .handle_sessions_command("/sessions", "telegram", "123", "telegram:123")
            .await;

        assert!(matches!(result, Some(DispatchResult::Handled)));
        let msg = rx.try_recv().unwrap();
        assert!(msg.content.contains("No sessions found"));
    }

    #[tokio::test]
    async fn should_return_none_for_non_sessions_command() {
        let (tx, _rx) = mpsc::channel(16);
        let (disp, _, _tmp) = setup_dispatcher(tx);

        let result = disp
            .handle_sessions_command("/new", "telegram", "123", "telegram:123")
            .await;

        assert!(result.is_none());
    }

    // ── /back tests ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn should_switch_back_to_previous_session() {
        let (tx, mut rx) = mpsc::channel(16);
        let (disp, _, _tmp) = setup_dispatcher(tx);
        let inbound = make_test_inbound("telegram", "123", "/back");

        disp.active_sessions
            .write()
            .await
            .switch_to("telegram:123", "research")
            .unwrap();

        let result = disp
            .handle_back_command("/back", &inbound, "telegram", "123", "telegram:123")
            .await;

        assert!(matches!(result, Some(DispatchResult::Handled)));
        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.content, "Switched back to session: (default)");
    }

    #[tokio::test]
    async fn should_handle_b_alias_for_back() {
        let (tx, mut rx) = mpsc::channel(16);
        let (disp, _, _tmp) = setup_dispatcher(tx);
        let inbound = make_test_inbound("telegram", "123", "/b");

        disp.active_sessions
            .write()
            .await
            .switch_to("telegram:123", "deep-search")
            .unwrap();

        let result = disp
            .handle_back_command("/b", &inbound, "telegram", "123", "telegram:123")
            .await;

        assert!(matches!(result, Some(DispatchResult::Handled)));
        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.content, "Switched back to session: (default)");
    }

    #[tokio::test]
    async fn should_report_no_previous_session() {
        let (tx, mut rx) = mpsc::channel(16);
        let (disp, _, _tmp) = setup_dispatcher(tx);
        let inbound = make_test_inbound("telegram", "123", "/back");

        let result = disp
            .handle_back_command("/back", &inbound, "telegram", "123", "telegram:123")
            .await;

        assert!(matches!(result, Some(DispatchResult::Handled)));
        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.content, "No previous session to switch to.");
    }

    // ── /delete tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn should_delete_named_session() {
        let (tx, mut rx) = mpsc::channel(16);
        let (disp, _, _tmp) = setup_dispatcher(tx);
        let inbound = make_test_inbound("telegram", "123", "/delete old");

        let result = disp
            .handle_delete_command("/delete old", &inbound, "telegram", "123", "telegram:123")
            .await;

        assert!(matches!(result, Some(DispatchResult::Handled)));
        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.content, "Deleted session: old");
    }

    #[tokio::test]
    async fn should_return_none_for_non_delete_command() {
        let (tx, _rx) = mpsc::channel(16);
        let (disp, _, _tmp) = setup_dispatcher(tx);
        let inbound = make_test_inbound("telegram", "123", "/new");

        let result = disp
            .handle_delete_command("/new", &inbound, "telegram", "123", "telegram:123")
            .await;

        assert!(result.is_none());
    }

    // ── Callback query tests ────────────────────────────────────────────

    #[tokio::test]
    async fn should_switch_session_via_callback() {
        let (tx, _rx) = mpsc::channel(16);
        let (disp, _, _tmp) = setup_dispatcher(tx);
        let inbound = make_test_inbound("telegram", "123", "");

        let result = disp
            .handle_session_callback(
                "s:coding",
                None,
                &inbound,
                "telegram",
                "123",
                "telegram:123",
                None,
            )
            .await;

        assert!(matches!(result, Some(DispatchResult::Handled)));

        let topic = disp
            .active_sessions
            .read()
            .await
            .get_active_topic("telegram:123")
            .to_string();
        assert_eq!(topic, "coding");
    }

    #[tokio::test]
    async fn should_switch_to_default_via_callback() {
        let (tx, _rx) = mpsc::channel(16);
        let (disp, _, _tmp) = setup_dispatcher(tx);
        let inbound = make_test_inbound("telegram", "123", "");

        disp.active_sessions
            .write()
            .await
            .switch_to("telegram:123", "research")
            .unwrap();

        let result = disp
            .handle_session_callback(
                "s:",
                None,
                &inbound,
                "telegram",
                "123",
                "telegram:123",
                None,
            )
            .await;

        assert!(matches!(result, Some(DispatchResult::Handled)));
        let topic = disp
            .active_sessions
            .read()
            .await
            .get_active_topic("telegram:123")
            .to_string();
        assert_eq!(topic, "");
    }

    #[tokio::test]
    async fn should_ignore_non_session_callback() {
        let (tx, _rx) = mpsc::channel(16);
        let (disp, _, _tmp) = setup_dispatcher(tx);
        let inbound = make_test_inbound("telegram", "123", "");

        let result = disp
            .handle_session_callback(
                "menu:settings",
                None,
                &inbound,
                "telegram",
                "123",
                "telegram:123",
                None,
            )
            .await;

        assert!(result.is_none());
    }

    // ── try_dispatch_session_command tests ───────────────────────────────

    #[tokio::test]
    async fn should_dispatch_session_commands_in_priority_order() {
        let (tx, mut rx) = mpsc::channel(16);
        let (disp, _, _tmp) = setup_dispatcher(tx);
        let inbound = make_test_inbound("telegram", "123", "/new test");
        let session_key = SessionKey::new("telegram", "123");

        let result = disp
            .try_dispatch_session_command(
                "/new test",
                &inbound,
                &session_key,
                "telegram",
                "123",
                "telegram:123",
            )
            .await;

        assert!(matches!(result, DispatchResult::Handled));
        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.content, "Switched to session: test");
    }

    #[tokio::test]
    async fn should_forward_unrecognized_commands() {
        let (tx, _rx) = mpsc::channel(16);
        let (disp, _, _tmp) = setup_dispatcher(tx);
        let inbound = make_test_inbound("telegram", "123", "hello world");
        let session_key = SessionKey::new("telegram", "123");

        let result = disp
            .try_dispatch_session_command(
                "hello world",
                &inbound,
                &session_key,
                "telegram",
                "123",
                "telegram:123",
            )
            .await;

        assert!(matches!(result, DispatchResult::Forward));
    }

    // ── Flush integration test ──────────────────────────────────────────

    #[tokio::test]
    async fn should_flush_pending_on_session_switch_via_s() {
        let (tx, mut rx) = mpsc::channel(16);
        let (disp, pending, _tmp) = setup_dispatcher(tx);
        let inbound = make_test_inbound("telegram", "123", "/s research");

        // Pre-populate pending messages for the target session (use profiled key)
        let target_key = build_profiled_session_key(None, "telegram", "123", "research");
        pending.lock().await.insert(
            target_key.to_string(),
            vec![make_reply("telegram", "123", "buffered deep search result")],
        );

        let result = disp
            .handle_s_command("/s research", &inbound, "telegram", "123", "telegram:123")
            .await;

        assert!(matches!(result, Some(DispatchResult::Handled)));

        // Should get the switch confirmation AND the flushed message
        let msg1 = rx.try_recv().unwrap();
        assert!(msg1.content.starts_with("Switched to session: research"));
        let msg2 = rx.try_recv().unwrap();
        assert_eq!(msg2.content, "buffered deep search result");
    }

    #[tokio::test]
    async fn should_flush_pending_on_callback_switch() {
        let (tx, mut rx) = mpsc::channel(16);
        let (disp, pending, _tmp) = setup_dispatcher(tx);
        let inbound = make_test_inbound("telegram", "123", "");

        // Pre-populate pending messages (use profiled key)
        let target_key = build_profiled_session_key(None, "telegram", "123", "deep");
        pending.lock().await.insert(
            target_key.to_string(),
            vec![make_reply("telegram", "123", "deep search report")],
        );

        let result = disp
            .handle_session_callback(
                "s:deep",
                None,
                &inbound,
                "telegram",
                "123",
                "telegram:123",
                None,
            )
            .await;

        assert!(matches!(result, Some(DispatchResult::Handled)));

        // The flushed message should be delivered
        let msg = rx.try_recv().unwrap();
        assert_eq!(msg.content, "deep search report");
    }

    #[tokio::test]
    async fn should_flush_pending_on_back_command() {
        let (tx, mut rx) = mpsc::channel(16);
        let (disp, pending, _tmp) = setup_dispatcher(tx);
        let inbound = make_test_inbound("telegram", "123", "/back");

        // Switch to research first, then back should go to default
        disp.active_sessions
            .write()
            .await
            .switch_to("telegram:123", "research")
            .unwrap();

        // Pre-populate pending for default session (use profiled key)
        let default_key = build_profiled_session_key(None, "telegram", "123", "");
        pending.lock().await.insert(
            default_key.to_string(),
            vec![make_reply("telegram", "123", "old pending msg")],
        );

        let result = disp
            .handle_back_command("/back", &inbound, "telegram", "123", "telegram:123")
            .await;

        assert!(matches!(result, Some(DispatchResult::Handled)));

        let msg1 = rx.try_recv().unwrap();
        assert_eq!(msg1.content, "Switched back to session: (default)");
        let msg2 = rx.try_recv().unwrap();
        assert_eq!(msg2.content, "old pending msg");
    }
}
