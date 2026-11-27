//! T-0653: the IM bridge against a real serve runtime and an existing chat,
//! with a scripted model and a fake channel.
use super::super::external_turns::tests::{Fixture, ScriptedLlm, restart, scripted, serve, unique};
use super::super::tests::test_support::*;
use super::*;

const BOT: i64 = 900;
/// A person, and their direct chat with the bot (Telegram gives both one id).
pub(crate) const ME: i64 = 4242;
pub(crate) const SOMEONE: i64 = 5151;
const TTL: Duration = Duration::from_secs(600);
const STOP_TOO_LATE: &str = "Nothing sent before this /stop is running.";

/// A channel that keeps what it was asked to send, and fails sends while
/// `fail` is set.
#[derive(Default)]
pub(crate) struct FakeChannel {
    sent: StdMutex<Vec<(i64, String, Option<i64>)>>,
    fail: std::sync::atomic::AtomicBool,
    /// Each `send_buttons`: the chat and its buttons' callback data.
    pub(crate) buttoned: StdMutex<Vec<(i64, Vec<String>)>>,
    /// Each callback ACK: whether it was an alert.
    pub(crate) acked: StdMutex<Vec<bool>>,
    pub(crate) fail_ack: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl ImChannel for FakeChannel {
    fn id(&self) -> ImChannelId {
        ImChannelId {
            platform: "fake".into(),
            bot: BOT,
        }
    }
    async fn send(&self, chat: i64, text: &str, reply_to: Option<i64>) -> eyre::Result<()> {
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            eyre::bail!("the platform did not answer");
        }
        self.sent
            .lock()
            .unwrap()
            .push((chat, text.into(), reply_to));
        Ok(())
    }

    fn buttons(&self) -> bool {
        true
    }

    async fn send_buttons(
        &self,
        chat: i64,
        _text: &str,
        _reply_to: Option<i64>,
        buttons: &[(String, String)],
    ) -> eyre::Result<Option<i64>> {
        let data = buttons.iter().map(|(_, data)| data.clone()).collect();
        self.buttoned.lock().unwrap().push((chat, data));
        Ok(Some(1000 + self.buttoned.lock().unwrap().len() as i64))
    }

    async fn edit(&self, chat: i64, _message_id: i64, text: &str) -> eyre::Result<()> {
        self.sent.lock().unwrap().push((chat, text.into(), None));
        Ok(())
    }

    async fn answer_callback(&self, _id: &str, _text: &str, alert: bool) -> eyre::Result<()> {
        if self.fail_ack.load(std::sync::atomic::Ordering::SeqCst) {
            eyre::bail!("the platform did not answer the callback");
        }
        self.acked.lock().unwrap().push(alert);
        Ok(())
    }
}

impl FakeChannel {
    pub(crate) fn take(&self) -> Vec<(i64, String, Option<i64>)> {
        std::mem::take(&mut self.sent.lock().unwrap())
    }
}

/// A text message from me in my chat, sent now.
pub(crate) fn message(id: i64, text: &str) -> ImMessage {
    ImMessage {
        chat: ME,
        direct: true,
        sender: ImSender::Person(ME),
        message_id: Some(id),
        sent_at: Some(Utc::now()),
        text: text.into(),
        attachments: 0,
    }
}

/// A served chat and its scripted model, on a temp profile.
async fn setup(key: &str) -> (tempfile::TempDir, Arc<ScriptedLlm>, Fixture, BoundChat) {
    let dir = tempfile::tempdir().unwrap();
    let llm = scripted();
    let fx = serve(dir.path(), llm.clone()).await;
    let chat = fx.chat(&unique(key)).await;
    (dir, llm, fx, chat)
}

fn binding(chat: i64, sender: i64, target: &BoundChat) -> ImBinding {
    ImBinding {
        channel: FakeChannel::default().id(),
        chat,
        sender,
        target: target.clone(),
    }
}

/// A bridge on `fx` with my chat bound to `target`.
fn bridge(
    fx: &Fixture,
    target: &BoundChat,
    approval_ttl: Duration,
) -> (Arc<ServeImBridge>, Arc<FakeChannel>) {
    let channel = Arc::new(FakeChannel::default());
    let turns = ExternalTurns::new(fx.state.clone(), fx.ledger.clone(), fx.contracts.clone());
    let bridge = ServeImBridge::new(
        turns,
        channel.clone(),
        vec![ME],
        vec![binding(ME, ME, target)],
        TTL,
        approval_ttl,
    );
    (bridge.unwrap(), channel)
}

async fn delivered(handled: Handled) -> Delivery {
    match handled {
        Handled::Started(delivery) => delivery.await.unwrap(),
        other => panic!("no turn started: {other:?}"),
    }
}

fn prompts(llm: &ScriptedLlm) -> Vec<String> {
    llm.prompts.lock().unwrap().clone()
}

#[tokio::test]
async fn a_bound_message_runs_once_and_its_result_goes_back_to_its_chat() {
    let dir = tempfile::tempdir().unwrap();
    let llm = scripted();
    let fx = serve(dir.path(), llm.clone()).await;
    let chat = fx.chat(&unique("im")).await;
    let (bridge, channel) = bridge(&fx, &chat, APPROVAL_TTL);

    assert_eq!(
        delivered(bridge.handle(message(1, "hello")).await).await,
        Delivery::Sent
    );
    assert_eq!(channel.take(), [(ME, "reply: hello".into(), Some(1))]);
    // The model sees the text alone, not where it came from.
    assert_eq!(prompts(&llm), ["hello"]);
    // A redelivery names the turn that ran.
    assert!(matches!(
        bridge.handle(message(1, "hello")).await,
        Handled::Duplicate(TurnRecord::Completed)
    ));
    assert_eq!(prompts(&llm), ["hello"]);
    assert!(channel.take().is_empty());
    // Delivered, the turn is forgotten.
    assert!(fx.entry.reconcile(&fx.profile).await.is_empty());

    let held = bridge.handle(message(2, "hold")).await;
    assert!(matches!(
        bridge.handle(message(3, "/status")).await,
        Handled::Answered
    ));
    assert!(matches!(
        bridge.handle(message(6, "/stop")).await,
        Handled::Answered
    ));
    assert_eq!(delivered(held).await, Delivery::Sent);
    // A message sent before the /stop arrives after it, then the /stop
    // again: its decision stands.
    let late = bridge.handle(message(5, "hold")).await;
    assert!(matches!(
        bridge.handle(message(6, "/stop")).await,
        Handled::Repeated
    ));
    // A /stop sent before that message, arriving late: it does not reach it.
    assert!(matches!(
        bridge.handle(message(4, "/stop")).await,
        Handled::Answered
    ));
    llm.release.add_permits(1);
    assert_eq!(delivered(late).await, Delivery::Sent);
    assert_eq!(
        channel.take(),
        [
            (ME, "A turn is running.".into(), Some(3)),
            (ME, "Stopped.".into(), Some(2)),
            (ME, STOP_TOO_LATE.into(), Some(4)),
            (ME, "reply: hold".into(), Some(5)),
        ]
    );
}

#[tokio::test]
async fn the_bridge_refuses_in_the_chat_what_it_cannot_tie_to_its_one_person() {
    let dir = tempfile::tempdir().unwrap();
    let llm = scripted();
    let fx = serve(dir.path(), llm.clone()).await;
    let chat = fx.chat(&unique("im")).await;
    let second = fx.chat(&unique("im-2")).await;
    let turns = || ExternalTurns::new(fx.state.clone(), fx.ledger.clone(), fx.contracts.clone());
    let elsewhere = ImBinding {
        channel: ImChannelId {
            platform: "fake".into(),
            bot: BOT + 1,
        },
        ..binding(ME, ME, &chat)
    };
    let refused = [
        (vec![], vec![binding(ME, ME, &chat)], "no allowed senders"),
        (vec![ME], vec![binding(ME, SOMEONE, &chat)], "is not allowed"),
        (vec![777_000], vec![binding(ME, 777_000, &chat)], "speaks for many"),
        (vec![ME], vec![elsewhere], "another channel"),
        (vec![ME], vec![binding(ME, ME, &chat), binding(ME, ME, &second)], "bound twice"),
        (vec![ME, SOMEONE], vec![binding(ME, ME, &chat), binding(SOMEONE, SOMEONE, &chat)], "bound to two chats"),
    ];
    for (allowed, bindings, why) in refused {
        let channel = Arc::new(FakeChannel::default());
        let error =
            ServeImBridge::new(turns(), channel, allowed, bindings, TTL, APPROVAL_TTL).err();
        assert!(
            error.as_deref().unwrap_or_default().contains(why),
            "{error:?}"
        );
    }

    let (bridge, channel) = bridge(&fx, &chat, APPROVAL_TTL);
    let refusals = [
        (
            ImMessage {
                chat: SOMEONE,
                ..message(1, "hi")
            },
            Refusal::UnknownChat,
        ),
        (
            ImMessage {
                direct: false,
                ..message(2, "hi")
            },
            Refusal::NotDirect,
        ),
        (
            ImMessage {
                sender: ImSender::Person(SOMEONE),
                ..message(3, "hi")
            },
            Refusal::NotAllowed,
        ),
        (
            ImMessage {
                sender: ImSender::Bot(ME),
                ..message(4, "hi")
            },
            Refusal::NotAllowed,
        ),
        (
            ImMessage {
                sender: ImSender::Chat(ME),
                ..message(5, "hi")
            },
            Refusal::NotAllowed,
        ),
        // A button press: no message id, and none made up.
        (
            ImMessage {
                message_id: None,
                ..message(6, "hi")
            },
            Refusal::NoMessageId,
        ),
        (
            ImMessage {
                sent_at: None,
                ..message(7, "hi")
            },
            Refusal::NoPlatformTime,
        ),
        (
            ImMessage {
                sent_at: Some(Utc::now() - TTL * 2),
                ..message(8, "hi")
            },
            Refusal::Stale,
        ),
        (
            ImMessage {
                attachments: 1,
                ..message(9, "hi")
            },
            Refusal::Attachment,
        ),
        (message(10, "  "), Refusal::Empty),
        (message(11, "/shell ls"), Refusal::UnknownCommand),
    ];
    let mut replies = Vec::new();
    for (message, refusal) in refusals {
        let chat = message.chat;
        match bridge.handle(message).await {
            Handled::Refused(got) => assert_eq!(got, refusal),
            other => panic!("{refusal:?} not refused: {other:?}"),
        }
        replies.push(chat);
    }
    let sent = channel.take();
    assert_eq!(
        sent.iter().map(|(chat, ..)| *chat).collect::<Vec<_>>(),
        replies
    );
    assert!(prompts(&llm).is_empty());
    assert!(fx.entry.reconcile(&fx.profile).await.is_empty());
}

#[tokio::test]
async fn a_revoked_chat_gets_no_result() {
    let dir = tempfile::tempdir().unwrap();
    let llm = scripted();
    let fx = serve(dir.path(), llm.clone()).await;
    let chat = fx.chat(&unique("im")).await;
    let (bridge, channel) = bridge(&fx, &chat, APPROVAL_TTL);

    let held = bridge.handle(message(1, "hold")).await;
    bridge.revoke(ME);
    llm.release.add_permits(1);
    assert_eq!(delivered(held).await, Delivery::Revoked);
    assert!(matches!(
        bridge.handle(message(2, "hello")).await,
        Handled::Refused(Refusal::UnknownChat)
    ));
    assert_eq!(
        channel.take(),
        [(ME, "This chat is not linked to octos.".into(), Some(2))]
    );
    assert_eq!(prompts(&llm), ["hold"]);
}

#[tokio::test]
async fn after_a_restart_a_redelivered_message_runs_nothing_again() {
    let dir = tempfile::tempdir().unwrap();
    let llm = scripted();
    let key = unique("im");
    {
        let fx = serve(dir.path(), llm.clone()).await;
        let chat = fx.chat(&key).await;
        let (bridge, channel) = bridge(&fx, &chat, APPROVAL_TTL);
        assert_eq!(
            delivered(bridge.handle(message(1, "before")).await).await,
            Delivery::Sent
        );
        // The platform did not take the result: it is not sent again, and
        // the turn stays recorded.
        channel
            .fail
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            delivered(bridge.handle(message(2, "lost")).await).await,
            Delivery::NotConfirmed
        );
        channel
            .fail
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let stopped = bridge.handle(message(4, "hold")).await;
        assert!(matches!(
            bridge.handle(message(5, "/stop")).await,
            Handled::Answered
        ));
        assert_eq!(delivered(stopped).await, Delivery::Sent);
        // Nothing is running: this /stop decides to stop nothing.
        assert!(matches!(
            bridge.handle(message(8, "/stop")).await,
            Handled::Answered
        ));
        // Message 3's turn started, then the server stopped.
        let cut_off = turn_id_for(&binding(ME, ME, &chat), 3);
        let started = octos_core::ui_protocol::TurnStartedEvent {
            session_id: key.clone(),
            turn_id: cut_off.clone(),
            timestamp: Utc::now(),
            topic: None,
        };
        fx.ledger
            .try_append_notification(UiNotification::TurnStarted(started))
            .unwrap();
    }
    let fx = restart(dir.path(), llm.clone()).await;
    let chat = BoundChat::verify(fx.profile.clone(), key.clone()).unwrap();
    let (bridge, channel) = bridge(&fx, &chat, APPROVAL_TTL);
    let recorded = fx.entry.reconcile(&fx.profile).await;
    assert_eq!(recorded.len(), 1);
    assert_eq!(*recorded[0].2.as_ref().unwrap(), TurnRecord::Completed);
    for (id, record) in [
        (1, TurnRecord::Completed),
        (2, TurnRecord::Completed),
        (3, TurnRecord::Orphaned),
        (4, TurnRecord::Interrupted),
    ] {
        match bridge.handle(message(id, "again")).await {
            Handled::Duplicate(got) => assert_eq!(got, record, "message {id}"),
            other => panic!("message {id} handled again: {other:?}"),
        }
    }
    // Both /stops delivered again after a message sent between them (and
    // delayed) started a turn: their decisions stand, and it runs.
    let late = bridge.handle(message(7, "hold")).await;
    for stop in [5, 8] {
        assert!(matches!(
            bridge.handle(message(stop, "/stop")).await,
            Handled::Repeated
        ));
    }
    llm.release.add_permits(1);
    assert_eq!(delivered(late).await, Delivery::Sent);
    assert_eq!(channel.take(), [(ME, "reply: hold".into(), Some(7))]);
    // Nothing ran twice; the stopped turn may end before it reaches the
    // model.
    let seen = prompts(&llm);
    assert_eq!(seen[..2], ["before", "lost"]);
    assert!(matches!(seen[2..].len(), 1 | 2) && seen[2..].iter().all(|p| p == "hold"));
}

#[tokio::test]
async fn a_deleted_chat_runs_nothing_more() {
    let dir = tempfile::tempdir().unwrap();
    let llm = scripted();
    let fx = serve(dir.path(), llm.clone()).await;
    let (first, second) = (fx.chat(&unique("im")).await, fx.chat(&unique("im-2")).await);
    // Another chat of mine.
    const MINE: i64 = 4343;
    let channel = Arc::new(FakeChannel::default());
    let turns = ExternalTurns::new(fx.state.clone(), fx.ledger.clone(), fx.contracts.clone());
    let bindings = vec![binding(ME, ME, &first), binding(MINE, ME, &second)];
    let bridge = ServeImBridge::new(
        turns,
        channel.clone(),
        vec![ME],
        bindings,
        TTL,
        APPROVAL_TTL,
    )
    .unwrap();
    let in_chat = |chat, id, text| ImMessage {
        chat,
        ..message(id, text)
    };
    for chat in [ME, MINE] {
        let handled = bridge.handle(in_chat(chat, 1, "hello")).await;
        assert_eq!(delivered(handled).await, Delivery::Sent);
    }

    // Each chat deleted while its message 2 is under way: the first while
    // its turn is admitted, the second after that and before it runs.
    let mut racing = Vec::new();
    for (chat, target, at) in [(ME, &first, ""), (MINE, &second, "/run")] {
        let pause = || Arc::new(tokio::sync::Notify::new());
        let (reached, release) = (pause(), pause());
        let turn = turn_id_for(&binding(chat, ME, target), 2);
        TURN_ADMISSION_TEST_PAUSES.lock().unwrap().insert(
            format!("{}{at}", turn.0),
            (reached.clone(), release.clone()),
        );
        let handled = tokio::spawn({
            let bridge = bridge.clone();
            async move { bridge.handle(in_chat(chat, 2, "racing")).await }
        });
        reached.notified().await;
        assert!(
            delete_session_as_admin(&fx.state, target.session_id())
                .await
                .is_success()
        );
        release.notify_one();
        racing.push(handled.await.unwrap());
    }
    assert!(matches!(
        racing[0],
        Handled::Refused(Refusal::NotStarted(_))
    ));
    // Admitted, it ends before the model or a tool runs.
    assert_eq!(delivered(racing.remove(1)).await, Delivery::Sent);
    // A message after the delete is refused: its chat is gone.
    match bridge.handle(in_chat(ME, 3, "later")).await {
        Handled::Refused(Refusal::NotStarted(why)) => assert!(why.contains("not a chat"), "{why}"),
        other => panic!("ran after the delete: {other:?}"),
    }
    assert_eq!(prompts(&llm), ["hello", "hello"]);
    assert!(
        channel
            .take()
            .contains(&(MINE, "The turn failed.".into(), Some(2)))
    );
    // Nothing stays recorded.
    assert!(fx.entry.reconcile(&fx.profile).await.is_empty());
}

#[tokio::test]
async fn a_stop_decision_lasts_while_its_stop_can_be_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let fx = serve(dir.path(), scripted()).await;
    let (first, second) = (fx.chat(&unique("im")).await, fx.chat(&unique("im-2")).await);
    // Another chat of mine.
    const MINE: i64 = 4343;
    let bridge = |chat, target, ttl| {
        let turns = ExternalTurns::new(fx.state.clone(), fx.ledger.clone(), fx.contracts.clone());
        let channel = Arc::new(FakeChannel::default());
        let bindings = vec![binding(chat, ME, target)];
        ServeImBridge::new(turns, channel, vec![ME], bindings, ttl, APPROVAL_TTL)
    };
    assert!(bridge(ME, &first, MAX_TTL + Duration::from_secs(1)).is_err());
    // On one profile: a bridge that accepts a message for a day, and one
    // that accepts it for half a second.
    let day = bridge(ME, &first, MAX_TTL).unwrap();
    let short = bridge(MINE, &second, Duration::from_millis(500)).unwrap();
    let stop = |chat, id, sent_at| ImMessage {
        chat,
        sent_at: Some(sent_at),
        ..message(id, "/stop")
    };
    let now = Utc::now();
    // Sent almost a day ago: accepted for 700 ms more.
    let nearly_a_day_ago = now - MAX_TTL + Duration::from_millis(700);
    // A platform clock 5 s ahead of this host's.
    let ahead = now + Duration::from_secs(5);
    for (bridge, message) in [
        (&short, stop(MINE, 1, now)),
        (&day, stop(ME, 1, nearly_a_day_ago)),
        (&short, stop(MINE, 2, ahead)),
    ] {
        assert!(matches!(bridge.handle(message).await, Handled::Answered));
    }
    tokio::time::sleep(Duration::from_millis(1100)).await;
    // The short bridge's next /stop forgets only the decision that no bridge
    // can accept any more.
    let next = short.handle(stop(MINE, 3, ahead)).await;
    assert!(matches!(next, Handled::Answered));
    let decisions = std::fs::read_dir(fx.profile.data_dir().join(STOPS_DIR));
    assert_eq!(decisions.unwrap().count(), 3);
    let stale = day.handle(stop(ME, 1, nearly_a_day_ago)).await;
    assert!(matches!(stale, Handled::Refused(Refusal::Stale)));
    // Past its own time to live, and past it twice over: reopened with a
    // longer time to live, the short bridge's /stops are still decided.
    let reopened = bridge(MINE, &second, TTL).unwrap();
    for message in [stop(MINE, 1, now), stop(MINE, 2, ahead)] {
        assert!(matches!(reopened.handle(message).await, Handled::Repeated));
    }
}

/// Start a held turn on `bridge`: its turn id, delivery task and owner
/// connection (the approvals the tests raise belong to it).
async fn held_turn(
    bridge: &Arc<ServeImBridge>,
    chat: &BoundChat,
) -> (TurnId, tokio::task::JoinHandle<Delivery>, u64) {
    let turn_id = turn_id_for(&binding(ME, ME, chat), 1);
    let Handled::Started(delivery) = bridge.handle(message(1, "hold")).await else {
        panic!("the turn did not start");
    };
    (
        turn_id,
        delivery,
        bridge.turns.test_connection().connection_id().0,
    )
}

/// The `at`-th projected approval's nonce (projections accumulate).
async fn projected(channel: &FakeChannel, at: usize) -> String {
    until_some("approval button projection", || {
        let buttoned = channel.buttoned.lock().unwrap();
        Some(buttoned.get(at)?.1[0].strip_prefix(CALLBACK_PREFIX)?[..16].to_owned())
    })
    .await
}

/// The first send/edit containing ANY needle; two needles cover racing
/// terminal texts ("cancelled" from the interrupt, the sweep's other text).
async fn sent_with(channel: &FakeChannel, needles: &[&str]) {
    until_some(&format!("channel send/edit containing {needles:?}"), || {
        let sent = channel.sent.lock().unwrap();
        sent.iter().any(|(_, t, _)| needles.iter().any(|n| t.contains(n))).then_some(())
    })
    .await;
}

#[tokio::test]
async fn approval_buttons_decide_once_and_everything_stale_fails_closed() {
    let (_dir, llm, fx, chat) = setup("im-approve").await;
    let (bridge, channel) = bridge(&fx, &chat, APPROVAL_TTL);
    let (turn_id, delivery, owner) = held_turn(&bridge, &chat).await;
    let raise = |risk| {
        raise_external_approval(&fx.contracts, &fx.ledger, chat.session_id(), &turn_id, owner, "shell", risk)
    };
    let approve = raise("low");
    let nonce = projected(&channel, 0).await;
    let mut wrong = im_press(ME, ImSender::Person(ME), &nonce, "a");
    wrong.sender = ImSender::Person(SOMEONE);
    bridge.handle_callback(wrong).await;
    bridge.handle_callback(im_press(ME, ImSender::Person(ME), "deadbeefdeadbeef", "a")).await;
    assert_eq!(channel.acked.lock().unwrap().as_slice(), [true, true]);
    assert!(!approve.is_finished());
    bridge.handle_callback(im_press(ME, ImSender::Person(ME), &nonce, "a")).await;
    assert_eq!(approve.await.unwrap(), ToolApprovalDecision::Approve);
    sent_with(&channel, &["Approved."]).await;
    bridge.handle_callback(im_press(ME, ImSender::Person(ME), &nonce, "a")).await;
    assert_eq!(channel.acked.lock().unwrap().len(), 4);
    channel
        .fail_ack
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let deny = raise("low");
    let nonce = projected(&channel, 1).await;
    bridge.handle_callback(im_press(ME, ImSender::Person(ME), &nonce, "d")).await;
    assert_eq!(deny.await.unwrap(), ToolApprovalDecision::Deny);
    sent_with(&channel, &["may not have gone through"]).await;
    llm.release.add_permits(1);
    assert_eq!(delivery.await.unwrap(), Delivery::Sent);
}

#[tokio::test]
async fn expired_stopped_and_risky_approvals_are_fail_closed() {
    let (_dir, llm, fx, chat) = setup("im-expire").await;
    let (bridge, channel) = bridge(&fx, &chat, Duration::from_millis(20));
    let (turn_id, delivery, owner) = held_turn(&bridge, &chat).await;
    let raise = |turn_id, risk| {
        raise_external_approval(&fx.contracts, &fx.ledger, chat.session_id(), turn_id, owner, "shell", risk)
    };
    let expired = raise(&turn_id, "low");
    let nonce = projected(&channel, 0).await;
    tokio::time::sleep(Duration::from_millis(60)).await;
    bridge.handle_callback(im_press(ME, ImSender::Person(ME), &nonce, "a")).await;
    sent_with(&channel, &["expired"]).await;
    drop(expired);
    llm.release.add_permits(1);
    delivery.await.unwrap();
    let turn_id = turn_id_for(&binding(ME, ME, &chat), 2);
    let Handled::Started(_) = bridge.handle(message(2, "hold")).await else {
        panic!("the turn did not start");
    };
    let risky = raise(&turn_id, "high");
    sent_with(&channel, &["high-risk"]).await;
    assert_eq!(channel.buttoned.lock().unwrap().len(), 1); // the expired one only
    bridge.handle_callback(im_press(ME, ImSender::Person(ME), "deadbeefdeadbeef", "a")).await;
    assert!(!risky.is_finished());
    let stopped = raise(&turn_id, "low");
    let nonce = projected(&channel, 1).await;
    bridge.handle(message(3, "/stop")).await;
    sent_with(&channel, &["no longer open", "was cancelled"]).await;
    bridge.handle_callback(im_press(ME, ImSender::Person(ME), &nonce, "a")).await;
    assert!(channel.acked.lock().unwrap().iter().all(|alert| *alert));
    drop(stopped);
}
