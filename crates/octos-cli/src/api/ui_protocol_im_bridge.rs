//! `ServeImBridge` (T-0653, `docs/IM_REMOTE_CONTROL.md` §14): a direct chat
//! with a bot on an IM platform drives one existing octos chat, through a
//! binding the operator made.
//!
//! The bridge starts its turns through [`ExternalTurns`], so they run in
//! serve's runtime on a ledger-only connection and are external: the external
//! tool allowlist, no session-wide approval from elsewhere, and `/stop`
//! reaches only the bridge's own turn. Where a message came from stays in
//! the bridge; the model and the session ledger see only its text.
//!
//! A message runs only if everything about it names one person in one bound
//! chat: the binding fixes the channel, the chat, the sender, the profile
//! and the session, all by stable ids. Anything else is answered in the chat
//! and nothing runs: an unknown chat or command, a group, a sender who is
//! not the binding's or not one person, a message without a platform id or
//! a platform time, one older than the time to live, an attachment.
//!
//! A message's turn id is derived from its channel, chat, sender and
//! platform message id, so a redelivery, before or after a restart, names
//! the turn that already ran and runs nothing.
//!
//! Nothing in `octos serve` starts a bridge yet: configuration (T-0654) and
//! a real channel do. The tests drive it with a fake channel. Attachments,
//! IM approvals and an outbox that redelivers results are later work.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::HashSet;

use sha2::{Digest, Sha256};

use super::external_turns::{BoundChat, ExternalTurns, Started, TurnRecord};
use super::*;

/// Telegram accounts that speak for many people: the service account that
/// forwards channel posts (777000), the anonymous group admin
/// (GroupAnonymousBot) and the channel signature (Channel_Bot). A binding to
/// one of them is a binding to whoever uses it.
const SHARED_SENDERS: [i64; 3] = [777_000, 1_087_968_824, 136_817_688];

/// A bot account on an IM platform: the channel a chat is on.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ImChannelId {
    pub(crate) platform: String,
    /// The platform's numeric id of the bot account.
    pub(crate) bot: i64,
}

/// The bridge's side of a channel: who it is, and how a reply goes out.
#[async_trait::async_trait]
pub(crate) trait ImChannel: Send + Sync {
    fn id(&self) -> ImChannelId;
    async fn send(&self, chat: i64, text: &str, reply_to: Option<i64>) -> eyre::Result<()>;
}

/// Who sent a message, as the platform tells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImSender {
    /// A person's account, by its numeric id.
    Person(i64),
    /// A bot's account.
    Bot(i64),
    /// Sent in the name of a chat, as an anonymous group admin or a channel
    /// post is: no one person sent it.
    Chat(i64),
}

/// One message the channel received.
#[derive(Debug, Clone)]
pub(crate) struct ImMessage {
    pub(crate) chat: i64,
    /// A one-to-one chat between the sender and the bot.
    pub(crate) direct: bool,
    pub(crate) sender: ImSender,
    /// The platform's id of the message in its chat. A button press has
    /// none (Telegram gives a callback no message id), and the bridge makes
    /// none up.
    pub(crate) message_id: Option<i64>,
    /// When the platform says it was sent; never when the bridge got it.
    pub(crate) sent_at: Option<DateTime<Utc>>,
    pub(crate) text: String,
    pub(crate) attachments: usize,
}

/// A chat on the bridge's channel, its one sender, and the octos chat they
/// drive.
#[derive(Debug, Clone)]
pub(crate) struct ImBinding {
    pub(crate) channel: ImChannelId,
    pub(crate) chat: i64,
    pub(crate) sender: i64,
    pub(crate) target: BoundChat,
}

/// What the bridge did with one message.
#[derive(Debug)]
pub(crate) enum Handled {
    /// Answered in the chat; nothing ran.
    Refused(Refusal),
    /// A turn with this message's id already exists; nothing ran and
    /// nothing was answered.
    Duplicate(TurnRecord),
    /// A command, answered in the chat (a `/stop` by the stopped turn's
    /// result).
    Answered,
    /// A turn started; `delivery` ends once its result has gone out, or
    /// could not.
    Started(tokio::task::JoinHandle<Delivery>),
}

/// Why a message was answered instead of run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// No binding for this chat, including one that was revoked.
    UnknownChat,
    /// Not a direct chat with the bot.
    NotDirect,
    /// Not the binding's sender, or not one person.
    NotAllowed,
    /// No platform message id to tell a redelivery from a new message.
    NoMessageId,
    NoPlatformTime,
    /// Sent longer ago than the time to live.
    Stale,
    Attachment,
    Empty,
    UnknownCommand,
    /// The chat refused the turn.
    NotStarted(String),
}

/// How a turn's result left the bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// The channel accepted the message.
    Sent,
    /// The channel failed the send. It may still have arrived, so it is not
    /// sent again here, and the turn stays recorded.
    NotConfirmed,
    /// The binding was revoked while the turn ran: nothing went out.
    Revoked,
}

pub(crate) struct ServeImBridge {
    turns: ExternalTurns,
    channel: Arc<dyn ImChannel>,
    allowed: HashSet<i64>,
    bindings: StdMutex<Vec<ImBinding>>,
    /// How old a message may be, by its platform time, and still run.
    ttl: Duration,
    /// The turn the bridge last started in each chat, while it runs.
    running: StdMutex<HashMap<SessionKey, TurnId>>,
}

impl ServeImBridge {
    /// Refuse bindings that would let a message reach anything the operator
    /// did not name for one person.
    pub(crate) fn new(
        turns: ExternalTurns,
        channel: Arc<dyn ImChannel>,
        allowed: Vec<i64>,
        bindings: Vec<ImBinding>,
        ttl: Duration,
    ) -> Result<Arc<Self>, String> {
        if allowed.is_empty() {
            return Err("no allowed senders; list who may use the bridge".into());
        }
        let mut chats = HashSet::new();
        let mut sessions = HashSet::new();
        for binding in &bindings {
            let chat = binding.chat;
            if binding.channel != channel.id() {
                return Err(format!("chat {chat}: bound on another channel"));
            }
            if SHARED_SENDERS.contains(&binding.sender) {
                return Err(format!(
                    "chat {chat}: sender {} speaks for many people",
                    binding.sender
                ));
            }
            if !allowed.contains(&binding.sender) {
                return Err(format!(
                    "chat {chat}: sender {} is not allowed",
                    binding.sender
                ));
            }
            if !chats.insert(chat) {
                return Err(format!("chat {chat} is bound twice"));
            }
            if !sessions.insert(binding.target.session_id().clone()) {
                return Err(format!(
                    "{} is bound to two chats",
                    binding.target.session_id().0
                ));
            }
        }
        Ok(Arc::new(Self {
            turns,
            channel,
            allowed: allowed.into_iter().collect(),
            bindings: StdMutex::new(bindings),
            ttl,
            running: StdMutex::default(),
        }))
    }

    /// Unbind `chat`. Its next message is refused, and the result of a turn
    /// still running for it is not sent.
    pub(crate) fn revoke(&self, chat: i64) {
        self.lock_bindings().retain(|binding| binding.chat != chat);
    }

    fn lock_bindings(&self) -> std::sync::MutexGuard<'_, Vec<ImBinding>> {
        self.bindings.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn binding(&self, chat: i64) -> Option<ImBinding> {
        let bindings = self.lock_bindings();
        bindings
            .iter()
            .find(|binding| binding.chat == chat)
            .cloned()
    }

    pub(crate) async fn handle(self: &Arc<Self>, message: ImMessage) -> Handled {
        let refusal = 'checks: {
            let Some(binding) = self.binding(message.chat) else {
                break 'checks Refusal::UnknownChat;
            };
            if !message.direct {
                break 'checks Refusal::NotDirect;
            }
            match message.sender {
                ImSender::Person(id) if id == binding.sender && self.allowed.contains(&id) => {}
                _ => break 'checks Refusal::NotAllowed,
            }
            let Some(message_id) = message.message_id else {
                break 'checks Refusal::NoMessageId;
            };
            let Some(sent_at) = message.sent_at else {
                break 'checks Refusal::NoPlatformTime;
            };
            if Utc::now()
                .signed_duration_since(sent_at)
                .to_std()
                .unwrap_or_default()
                > self.ttl
            {
                break 'checks Refusal::Stale;
            }
            if message.attachments > 0 {
                break 'checks Refusal::Attachment;
            }
            let text = message.text.trim();
            if text.is_empty() {
                break 'checks Refusal::Empty;
            }
            return match text.strip_prefix('/') {
                Some(command) => self.command(&binding, message_id, command).await,
                None => self.start(binding, message_id, text).await,
            };
        };
        self.refuse(message.chat, message.message_id, refusal).await
    }

    async fn command(&self, binding: &ImBinding, message_id: i64, command: &str) -> Handled {
        let running = self.running_turn(binding.target.session_id());
        let reply = match (command.split_whitespace().next(), running) {
            (Some("status"), Some(_)) => "A turn is running.",
            (Some("status" | "stop"), None) => "Nothing is running.",
            // The stopped turn's result answers.
            (Some("stop"), Some(turn_id)) => {
                match self.turns.interrupt(&binding.target, turn_id).await {
                    Ok(_) => return Handled::Answered,
                    Err(_) => "Could not stop the turn.",
                }
            }
            _ => {
                return self
                    .refuse(binding.chat, Some(message_id), Refusal::UnknownCommand)
                    .await;
            }
        };
        self.send(binding.chat, reply, Some(message_id)).await;
        Handled::Answered
    }

    async fn start(self: &Arc<Self>, binding: ImBinding, message_id: i64, text: &str) -> Handled {
        let turn_id = turn_id_for(&binding, message_id);
        let events = self.turns.subscribe(&binding.target);
        match self
            .turns
            .start(&binding.target, turn_id.clone(), text)
            .await
        {
            Ok(Started::Now) => {}
            Ok(Started::Already(record)) => return Handled::Duplicate(record),
            Err(error) => {
                let refusal = Refusal::NotStarted(error.message);
                return self.refuse(binding.chat, Some(message_id), refusal).await;
            }
        }
        let session = binding.target.session_id().clone();
        self.lock_running().insert(session, turn_id.clone());
        let bridge = self.clone();
        Handled::Started(tokio::spawn(async move {
            let outcome = bridge.turns.ended(&binding.target, &turn_id, events).await;
            bridge.deliver(binding, message_id, turn_id, outcome).await
        }))
    }

    async fn deliver(
        &self,
        binding: ImBinding,
        message_id: i64,
        turn_id: TurnId,
        outcome: Result<(TurnRecord, Option<String>), RpcError>,
    ) -> Delivery {
        let session = binding.target.session_id();
        {
            let mut running = self.lock_running();
            if running.get(session) == Some(&turn_id) {
                running.remove(session);
            }
        }
        let delivery = if self.binding(binding.chat).is_none() {
            Delivery::Revoked
        } else {
            let content = match outcome {
                Ok((TurnRecord::Completed, Some(reply))) => reply,
                Ok((TurnRecord::Completed, None)) => "Done (no reply).".into(),
                Ok((TurnRecord::Interrupted, _)) => "Stopped.".into(),
                Ok((TurnRecord::Failed, _)) => "The turn failed.".into(),
                Ok(_) | Err(_) => "The result is unknown; check the chat in octos.".into(),
            };
            match self
                .channel
                .send(binding.chat, &content, Some(message_id))
                .await
            {
                Ok(()) => Delivery::Sent,
                Err(error) => {
                    tracing::warn!(
                        chat = binding.chat,
                        turn_id = %turn_id.0,
                        %error,
                        "IM result send failed; it may still have arrived, so it is not resent"
                    );
                    Delivery::NotConfirmed
                }
            }
        };
        if delivery != Delivery::NotConfirmed {
            let profile = binding.target.profile();
            if let Err(error) = self.turns.acknowledge(profile, session, &turn_id).await {
                tracing::warn!(turn_id = %turn_id.0, error = %error.message, "IM turn not forgotten");
            }
        }
        delivery
    }

    fn lock_running(&self) -> std::sync::MutexGuard<'_, HashMap<SessionKey, TurnId>> {
        self.running.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn running_turn(&self, session: &SessionKey) -> Option<TurnId> {
        self.lock_running().get(session).cloned()
    }

    async fn refuse(&self, chat: i64, message_id: Option<i64>, refusal: Refusal) -> Handled {
        let reply = match &refusal {
            Refusal::UnknownChat => "This chat is not linked to octos.".to_owned(),
            Refusal::NotDirect => "Only a direct chat with this bot can be used.".to_owned(),
            Refusal::NotAllowed => "You are not allowed to use this chat.".to_owned(),
            Refusal::NoMessageId => "This kind of message is not supported here.".to_owned(),
            Refusal::NoPlatformTime => "This message has no send time; not run.".to_owned(),
            Refusal::Stale => "This message is too old to run; send it again.".to_owned(),
            Refusal::Attachment => "Attachments are not supported yet; send text.".to_owned(),
            Refusal::Empty => "Send some text.".to_owned(),
            Refusal::UnknownCommand => {
                "Unknown command. The commands are /status and /stop.".to_owned()
            }
            Refusal::NotStarted(reason) => format!("Could not start: {reason}"),
        };
        self.send(chat, &reply, message_id).await;
        Handled::Refused(refusal)
    }

    async fn send(&self, chat: i64, text: &str, reply_to: Option<i64>) {
        if let Err(error) = self.channel.send(chat, text, reply_to).await {
            tracing::warn!(chat, %error, "IM reply send failed");
        }
    }
}

/// The turn id of a message: its channel, chat, sender and platform id
/// always name the same turn.
fn turn_id_for(binding: &ImBinding, message_id: i64) -> TurnId {
    let mut hash = Sha256::new();
    hash.update(binding.channel.platform.len().to_le_bytes());
    hash.update(binding.channel.platform.as_bytes());
    for part in [
        binding.channel.bot,
        binding.chat,
        binding.sender,
        message_id,
    ] {
        hash.update(part.to_le_bytes());
    }
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash.finalize()[..16]);
    TurnId(uuid::Builder::from_custom_bytes(bytes).into_uuid())
}

#[cfg(test)]
#[path = "ui_protocol_im_bridge_tests.rs"]
mod tests;
