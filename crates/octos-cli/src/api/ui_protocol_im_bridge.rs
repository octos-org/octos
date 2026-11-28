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
//! `octos serve` makes a bridge for each `serve.im` bot (T-0654), fed by
//! the bot's Telegram transport (T-0663). The tests here drive it with a
//! fake channel. Attachments, IM approvals and an outbox that redelivers
//! results are later work.
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

/// Where a profile keeps the bridge's `/stop` decisions, one file each,
/// while a delivery of the `/stop` could still be run.
const STOPS_DIR: &str = "im_bridge_stops";

/// The longest time to live a bridge may have: Telegram keeps an update the
/// bot has not confirmed for 24 hours. A `/stop` decision is kept this long
/// after its platform time, so no bridge on the profile, whatever its time
/// to live, now or after a restart, accepts a `/stop` whose decision is gone.
pub(crate) const MAX_TTL: Duration = Duration::from_secs(24 * 60 * 60);

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
    /// Whether approvals may be shown as buttons. Channels without buttons
    /// never get an approval projected: their users answer in the PWA.
    fn buttons(&self) -> bool {
        false
    }
    /// Send `text` with one row of `(label, callback data)` buttons,
    /// returning the platform's id of the sent message for later edits.
    async fn send_buttons(
        &self,
        _chat: i64,
        _text: &str,
        _reply_to: Option<i64>,
        _buttons: &[(String, String)],
    ) -> eyre::Result<Option<i64>> {
        eyre::bail!("this channel has no buttons")
    }
    /// Replace a message this channel sent (invalidate its buttons).
    async fn edit(&self, _chat: i64, _message_id: i64, _text: &str) -> eyre::Result<()> {
        eyre::bail!("this channel cannot edit")
    }
    /// ACK a button press. A failure is the caller's to surface truthfully;
    /// the ACK is never retried blind.
    async fn answer_callback(
        &self,
        _callback_id: &str,
        _text: &str,
        _alert: bool,
    ) -> eyre::Result<()> {
        eyre::bail!("this channel has no callbacks")
    }
    /// Send a turn's result.
    async fn send(&self, chat: i64, text: &str, reply_to: Option<i64>) -> eyre::Result<()>;
    /// Answer a message while it is handled: a refusal or a command's reply.
    /// The messages after it wait on this, so a channel that waits out rate
    /// limits in `send` does not here (T-0665).
    async fn answer(&self, chat: i64, text: &str, reply_to: Option<i64>) -> eyre::Result<()> {
        self.send(chat, text, reply_to).await
    }
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
    /// The platform's id of the message in its chat, which grows with the
    /// order messages were sent in (Telegram message ids and Discord
    /// snowflakes do). A button press has none (Telegram gives a callback no
    /// message id), and the bridge makes none up.
    pub(crate) message_id: Option<i64>,
    /// When the platform says it was sent; never when the bridge got it.
    pub(crate) sent_at: Option<DateTime<Utc>>,
    pub(crate) text: String,
    pub(crate) attachments: usize,
}

/// A button press on a bridge message. The platform gives no message id
/// the press could be confused with; the callback data's nonce, not any
/// id, names the approval.
#[derive(Debug, Clone)]
pub(crate) struct ImCallback {
    pub(crate) chat: i64,
    pub(crate) sender: ImSender,
    /// The platform's id of the press, for the ACK.
    pub(crate) callback_id: String,
    /// The button message's platform id, used only to edit that message.
    pub(crate) message_id: Option<i64>,
    pub(crate) data: String,
}

/// How long a projected approval answers button presses (T-0690).
pub(crate) const APPROVAL_TTL: Duration = Duration::from_secs(600);

/// The callback data of an approval button: `imp:<nonce>:a` (approve) or
/// `imp:<nonce>:d` (deny). The nonce is random, one-time and bound to the
/// approval id in the bridge's table; nothing in it is trusted alone.
pub(crate) const CALLBACK_PREFIX: &str = "imp:";

/// One approval the bridge offered as buttons, by its one-time nonce.
#[derive(Debug, Clone)]
pub(crate) struct PendingButton {
    pub(crate) approval_id: ApprovalId,
    pub(crate) chat: i64,
    pub(crate) session_id: SessionKey,
    pub(crate) turn_id: TurnId,
    /// The button message's platform id, once the channel told it.
    pub(crate) message_id: Option<i64>,
    /// When the bridge saw the approval; TTL counts from the server's event.
    pub(crate) observed_at: DateTime<Utc>,
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
    /// A `/stop` already decided; nothing was done again and nothing was
    /// answered.
    Repeated,
    /// Not answered: a chat that is neither bound nor direct was already told
    /// it is not linked, and telling everyone in it again is noise.
    Ignored,
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
    /// The turn the bridge last started in each chat, while it runs, and
    /// the id of the message that started it.
    running: StdMutex<HashMap<SessionKey, (TurnId, i64)>>,
    /// The chats, neither bound nor direct, already told they are not
    /// linked: each is told once while the bridge runs (T-0665).
    told_unlinked: StdMutex<HashSet<i64>>,
    /// Projected approvals by their one-time callback nonce (T-0690).
    buttons: StdMutex<HashMap<String, PendingButton>>,
    /// How long a projected approval answers a button press.
    approval_ttl: Duration,
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
        approval_ttl: Duration,
    ) -> Result<Arc<Self>, String> {
        if allowed.is_empty() {
            return Err("no allowed senders; list who may use the bridge".into());
        }
        if ttl > MAX_TTL {
            return Err("the time to live is over 24 hours".into());
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
            told_unlinked: StdMutex::default(),
            buttons: StdMutex::default(),
            approval_ttl,
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
                let told = |chat| {
                    let mut told = self.told_unlinked.lock().unwrap_or_else(|p| p.into_inner());
                    !told.insert(chat)
                };
                if !message.direct && told(message.chat) {
                    return Handled::Ignored;
                }
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
                Some(command) => self.command(&binding, message_id, sent_at, command).await,
                None => self.start(binding, message_id, text).await,
            };
        };
        self.refuse(message.chat, message.message_id, refusal).await
    }

    async fn command(
        &self,
        binding: &ImBinding,
        message_id: i64,
        sent_at: DateTime<Utc>,
        command: &str,
    ) -> Handled {
        let reply = match command.split_whitespace().next() {
            Some("status") => match self.running_turn(binding.target.session_id()) {
                Some(_) => "A turn is running.",
                None => "Nothing is running.",
            },
            Some("stop") => match self.stop(binding, message_id, sent_at).await {
                Ok(Some(reply)) => reply,
                // Stopped: the stopped turn's result answers.
                Ok(None) => return Handled::Answered,
                Err(Repeated) => return Handled::Repeated,
            },
            _ => {
                return self
                    .refuse(binding.chat, Some(message_id), Refusal::UnknownCommand)
                    .await;
            }
        };
        self.answer(binding.chat, reply, Some(message_id)).await;
        Handled::Answered
    }

    /// Decide what the /stop `message_id` stops, once: the decision is on
    /// disk before it is acted on, and a delivery of the same /stop finds it
    /// there and does nothing again, before or after a restart. Only a turn
    /// an earlier message started can be stopped, so a /stop that arrives
    /// late cannot reach a turn sent after it. The decision is kept until
    /// its /stop can no longer be accepted. Returns the reply, if any.
    async fn stop(
        &self,
        binding: &ImBinding,
        message_id: i64,
        sent_at: DateTime<Utc>,
    ) -> Result<Option<&'static str>, Repeated> {
        let target = self
            .running_turn(binding.target.session_id())
            .filter(|(_, started_by)| *started_by < message_id)
            .map(|(turn_id, _)| turn_id);
        let decisions = binding.target.profile().data_dir().join(STOPS_DIR);
        let decision = decisions.join(turn_id_for(binding, message_id).0.to_string());
        // After this no bridge accepts the /stop (see `handle`), whatever
        // its time to live: none may have more than `MAX_TTL`.
        let deadline = chrono::TimeDelta::from_std(MAX_TTL)
            .ok()
            .and_then(|ttl| sent_at.checked_add_signed(ttl))
            .unwrap_or(DateTime::<Utc>::MAX_UTC);
        match record_stop(&decisions, &decision, deadline, target.as_ref()) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(Repeated);
            }
            Err(error) => {
                tracing::warn!(%error, "IM /stop decision not recorded; nothing stopped");
                return Ok(Some("Could not stop the turn."));
            }
        }
        forget_expired_stops(&decisions);
        let Some(turn_id) = target else {
            return Ok(Some("Nothing sent before this /stop is running."));
        };
        match self.turns.interrupt(&binding.target, turn_id).await {
            Ok(_) => Ok(None),
            Err(_) => Ok(Some("Could not stop the turn.")),
        }
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
        self.lock_running()
            .insert(session, (turn_id.clone(), message_id));
        let bridge = self.clone();
        Handled::Started(tokio::spawn(async move {
            let outcome = bridge.watch_turn(&binding, &turn_id, events).await;
            // The turn is over: its unanswered approvals are moot (the store
            // cancels them); their buttons must not stay tappable.
            bridge
                .invalidate_turn_buttons(&turn_id, "This approval is no longer open.")
                .await;
            bridge.deliver(binding, message_id, turn_id, outcome).await
        }))
    }

    /// Follow the turn's ledger events, projecting its approval requests as
    /// buttons and invalidating them as they resolve, until the turn ends.
    /// Same end semantics as `ExternalTurns::ended`, plus the approval watch.
    async fn watch_turn(
        &self,
        binding: &ImBinding,
        turn_id: &TurnId,
        mut events: tokio::sync::broadcast::Receiver<LedgeredUiProtocolEvent>,
    ) -> Result<(TurnRecord, Option<String>), RpcError> {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match events.recv().await {
                Ok(event) => match event.event {
                    UiProtocolLedgerEvent::Notification(UiNotification::ApprovalRequested(e))
                        if e.turn_id == *turn_id =>
                    {
                        self.project_approval(binding, e).await
                    }
                    UiProtocolLedgerEvent::Notification(UiNotification::ApprovalDecided(e)) => {
                        let text = match e.decision {
                            ApprovalDecision::Approve => "Approved.",
                            _ => "Denied.",
                        };
                        self.invalidate_button(&e.approval_id, text).await;
                    }
                    UiProtocolLedgerEvent::Notification(UiNotification::ApprovalCancelled(e)) => {
                        self.invalidate_button(&e.approval_id, "This approval was cancelled.")
                            .await;
                    }
                    UiProtocolLedgerEvent::Notification(UiNotification::TurnCompleted(e))
                        if e.turn_id == *turn_id => {}
                    UiProtocolLedgerEvent::Notification(UiNotification::TurnError(e))
                        if e.turn_id == *turn_id => {}
                    _ => continue,
                },
                // Its end may be among the events missed.
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => {
                    return Err(RpcError::internal_error(
                        "the session's ledger closed before the turn ended",
                    ));
                }
            }
            let chat = &binding.target;
            match self.turns.record(chat, turn_id).await? {
                TurnRecord::NotStarted | TurnRecord::Running => continue,
                ended => break Ok((ended, self.turns.reply(chat, turn_id)?)),
            }
        }
    }

    /// Offer a pending approval of the running turn as buttons, or, for a
    /// high-risk one (permission tiers, network, writes outside the
    /// workspace, credentials/connectors), only a pointer to the PWA. A send
    /// failure drops the projection; the approval stays pending in the PWA.
    async fn project_approval(&self, binding: &ImBinding, event: ApprovalRequestedEvent) {
        let text = format!("Approval requested: {}\n{}", event.title, event.body);
        // Buttons only for a manifest-attested low/medium risk. "unspecified"
        // (the manifest said nothing), "high" and "max" — permission tiers,
        // network, writes outside the workspace, credentials/connectors —
        // never get an IM answer path: the PWA only.
        if !matches!(event.risk.as_deref(), Some("low" | "medium")) {
            let guidance = format!("{text}\nThis is a high-risk approval; answer it in octos.");
            self.answer_or_send(binding.chat, &guidance, None).await;
            return;
        }
        if !self.channel.buttons() {
            let guidance = format!("{text}\nAnswer this approval in octos.");
            self.answer_or_send(binding.chat, &guidance, None).await;
            return;
        }
        let nonce = format!(
            "{:x}",
            sha2::Sha256::digest(
                format!("{}:{}", event.approval_id.0, uuid::Uuid::new_v4()).as_bytes()
            )
        );
        let nonce = nonce[..16].to_owned();
        let entry = PendingButton {
            approval_id: event.approval_id.clone(),
            chat: binding.chat,
            session_id: binding.target.session_id().clone(),
            turn_id: event.turn_id.clone(),
            message_id: None,
            observed_at: Utc::now(),
        };
        self.lock_buttons().insert(nonce.clone(), entry);
        let buttons = [
            ("Approve".to_owned(), format!("{CALLBACK_PREFIX}{nonce}:a")),
            ("Deny".to_owned(), format!("{CALLBACK_PREFIX}{nonce}:d")),
        ];
        match self
            .channel
            .send_buttons(binding.chat, &text, None, &buttons)
            .await
        {
            Ok(message_id) => {
                let mut buttons = self.lock_buttons();
                if let Some(entry) = buttons.get_mut(&nonce) {
                    entry.message_id = message_id;
                }
            }
            Err(error) => {
                self.lock_buttons().remove(&nonce);
                tracing::warn!(chat = binding.chat, %error, "IM approval projection not sent");
            }
        }
    }

    /// Replace a projected approval's message with `text` and forget it:
    /// answered elsewhere, cancelled with its turn, or expired.
    fn take_button(&self, approval_id: &ApprovalId) -> Option<PendingButton> {
        let mut buttons = self.lock_buttons();
        let nonce = buttons
            .iter()
            .find(|(_, e)| e.approval_id == *approval_id)
            .map(|(nonce, _)| nonce.clone())?;
        buttons.remove(&nonce)
    }

    async fn invalidate_button(&self, approval_id: &ApprovalId, text: &str) {
        let Some(entry) = self.take_button(approval_id) else {
            return;
        };
        if let Some(message_id) = entry.message_id {
            if let Err(error) = self.channel.edit(entry.chat, message_id, text).await {
                tracing::warn!(chat = entry.chat, %error, "IM approval message not invalidated");
            }
        }
    }

    async fn invalidate_turn_buttons(&self, turn_id: &TurnId, text: &str) {
        let stale: Vec<PendingButton> = self
            .lock_buttons()
            .values()
            .filter(|entry| entry.turn_id == *turn_id)
            .cloned()
            .collect();
        for entry in stale {
            self.invalidate_button(&entry.approval_id, text).await;
        }
    }

    /// Expire projected approvals past their TTL, editing their messages.
    /// The sweeper calls this; tests call it directly.
    pub(crate) async fn expire_stale_buttons(&self) {
        let now = Utc::now();
        let stale: Vec<PendingButton> = self
            .lock_buttons()
            .values()
            .filter(|entry| {
                now.signed_duration_since(entry.observed_at)
                    .to_std()
                    .is_ok_and(|age| age > self.approval_ttl)
            })
            .cloned()
            .collect();
        for entry in stale {
            self.invalidate_button(&entry.approval_id, "This approval expired.")
                .await;
        }
    }

    /// Answer a button press (T-0690). Every check is re-read: the binding,
    /// the sender, the nonce's entry, the running turn, the TTL, and the
    /// server's own approval state through `respond_approval`'s typed errors.
    /// Anything off is answered loudly and changes nothing.
    pub(crate) async fn handle_callback(&self, callback: ImCallback) {
        let refused = 'checks: {
            let Some(binding) = self.binding(callback.chat) else {
                break 'checks "This chat is not linked to octos.";
            };
            match callback.sender {
                ImSender::Person(id) if id == binding.sender && self.allowed.contains(&id) => {}
                _ => break 'checks "You are not allowed to answer this approval.",
            }
            let Some(rest) = callback.data.strip_prefix(CALLBACK_PREFIX) else {
                break 'checks "This approval is no longer valid.";
            };
            let Some((nonce, decision)) = rest.rsplit_once(':') else {
                break 'checks "This approval is no longer valid.";
            };
            let Some(entry) = self.lock_buttons().get(nonce).cloned() else {
                break 'checks "This approval is no longer valid.";
            };
            if entry.chat != callback.chat
                || entry.session_id != *binding.target.session_id()
                || self
                    .running_turn(&entry.session_id)
                    .is_none_or(|(running, _)| running != entry.turn_id)
            {
                self.invalidate_button(&entry.approval_id, "This approval is no longer open.")
                    .await;
                break 'checks "This approval is no longer open.";
            }
            if Utc::now()
                .signed_duration_since(entry.observed_at)
                .to_std()
                .is_ok_and(|age| age > self.approval_ttl)
            {
                self.invalidate_button(&entry.approval_id, "This approval expired.")
                    .await;
                break 'checks "This approval expired; answer it in octos if it is still open.";
            }
            let decision = match decision {
                "a" => ApprovalDecision::Approve,
                "d" => ApprovalDecision::Deny,
                _ => break 'checks "This approval is no longer valid.",
            };
            let params = octos_core::ui_protocol::ApprovalRespondParams {
                session_id: entry.session_id.clone(),
                approval_id: entry.approval_id.clone(),
                decision: decision.clone(),
                approval_scope: None,
                client_note: None,
            };
            match self.turns.respond_approval(&binding.target, params).await {
                Ok(_) => {
                    let text = if decision == ApprovalDecision::Approve {
                        "Approved."
                    } else {
                        "Denied."
                    };
                    // The server decided it: the button message shows the
                    // truth immediately, then the press is ACKed. An ACK the
                    // channel never answered leaves the spinner — say so;
                    // the message already shows the final state.
                    self.invalidate_button(&entry.approval_id, text).await;
                    if let Err(error) = self
                        .channel
                        .answer_callback(&callback.callback_id, text, false)
                        .await
                    {
                        tracing::warn!(chat = callback.chat, %error, "IM approval ACK failed");
                        self.answer_or_send(
                            callback.chat,
                            "The answer was recorded; this notice may not have gone through — the approval message shows the final state.",
                            None,
                        )
                        .await;
                    }
                    return;
                }
                Err(error) => {
                    tracing::warn!(
                        chat = callback.chat,
                        error = %error.message,
                        "IM approval answer refused by the server"
                    );
                    self.invalidate_button(&entry.approval_id, "This approval is no longer open.")
                        .await;
                    break 'checks "This approval is no longer open.";
                }
            }
        };
        // Loud, and nothing was decided.
        if let Err(error) = self
            .channel
            .answer_callback(&callback.callback_id, refused, true)
            .await
        {
            tracing::warn!(chat = callback.chat, %error, "IM refusal callback ACK failed");
        }
    }

    async fn answer_or_send(&self, chat: i64, text: &str, reply_to: Option<i64>) {
        if let Err(error) = self.channel.send(chat, text, reply_to).await {
            tracing::warn!(chat, %error, "IM message not sent");
        }
    }

    fn lock_buttons(&self) -> std::sync::MutexGuard<'_, HashMap<String, PendingButton>> {
        self.buttons.lock().unwrap_or_else(|p| p.into_inner())
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
            if running
                .get(session)
                .is_some_and(|(running, _)| *running == turn_id)
            {
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

    fn lock_running(&self) -> std::sync::MutexGuard<'_, HashMap<SessionKey, (TurnId, i64)>> {
        self.running.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn running_turn(&self, session: &SessionKey) -> Option<(TurnId, i64)> {
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
        self.answer(chat, &reply, message_id).await;
        Handled::Refused(refusal)
    }

    async fn answer(&self, chat: i64, text: &str, reply_to: Option<i64>) {
        if let Err(error) = self.channel.answer(chat, text, reply_to).await {
            tracing::warn!(chat, %error, "IM reply send failed");
        }
    }
}

/// A `/stop` whose decision was already made.
struct Repeated;

/// Record a `/stop`'s decision, the turn it stops or none, under the
/// deadline after which its `/stop` is no longer accepted. The file is
/// written whole and then linked into place, which fails if a decision is
/// there already, so of two deliveries only one decides.
fn record_stop(
    dir: &Path,
    path: &Path,
    deadline: DateTime<Utc>,
    target: Option<&TurnId>,
) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let decision = target.map_or_else(|| "nothing".to_owned(), |turn| format!("stop {}", turn.0));
    let draft = dir.join(format!(".{}.draft", uuid::Uuid::new_v4().simple()));
    let mut file = std::fs::File::create(&draft)?;
    let written = writeln!(file, "{}\n{decision}", deadline.to_rfc3339())
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::hard_link(&draft, path));
    let _ = std::fs::remove_file(&draft);
    written
}

/// Forget the decisions whose `/stop` can no longer be accepted: a delivery
/// of it now is refused as stale before it reaches them. Each decision keeps
/// its own deadline, its platform time plus `MAX_TTL`, so neither the time to
/// live of the bridge reading them nor its clock matters.
fn forget_expired_stops(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = Utc::now();
    for entry in entries.flatten() {
        let expired = std::fs::read_to_string(entry.path())
            .ok()
            .and_then(|decision| {
                let deadline = decision.lines().next()?;
                DateTime::parse_from_rfc3339(deadline).ok()
            })
            .is_some_and(|deadline| deadline < now);
        if expired {
            let _ = std::fs::remove_file(entry.path());
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
pub(super) mod tests;
