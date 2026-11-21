//! T-0653: the IM bridge against a real serve runtime and an existing chat,
//! with a scripted model and a fake channel.
use super::super::external_turns::tests::{Fixture, ScriptedLlm, restart, scripted, serve, unique};
use super::super::tests::test_support::*;
use super::*;

const BOT: i64 = 900;
/// A person, and their direct chat with the bot (Telegram gives both one id).
const ME: i64 = 4242;
const SOMEONE: i64 = 5151;
const TTL: Duration = Duration::from_secs(600);
const STOP_TOO_LATE: &str = "Nothing sent before this /stop is running.";

/// A channel that keeps what it was asked to send, and fails sends while
/// `fail` is set.
#[derive(Default)]
struct FakeChannel {
    sent: StdMutex<Vec<(i64, String, Option<i64>)>>,
    fail: std::sync::atomic::AtomicBool,
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
}

impl FakeChannel {
    fn take(&self) -> Vec<(i64, String, Option<i64>)> {
        std::mem::take(&mut self.sent.lock().unwrap())
    }
}

/// A text message from me in my chat, sent now.
fn message(id: i64, text: &str) -> ImMessage {
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

fn binding(chat: i64, sender: i64, target: &BoundChat) -> ImBinding {
    ImBinding {
        channel: FakeChannel::default().id(),
        chat,
        sender,
        target: target.clone(),
    }
}

/// A bridge on `fx` with my chat bound to `target`.
fn bridge(fx: &Fixture, target: &BoundChat) -> (Arc<ServeImBridge>, Arc<FakeChannel>) {
    let channel = Arc::new(FakeChannel::default());
    let turns = ExternalTurns::new(fx.state.clone(), fx.ledger.clone(), fx.contracts.clone());
    let bridge = ServeImBridge::new(
        turns,
        channel.clone(),
        vec![ME],
        vec![binding(ME, ME, target)],
        TTL,
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
    let (bridge, channel) = bridge(&fx, &chat);

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
        bridge.handle(message(4, "/stop")).await,
        Handled::Answered
    ));
    assert_eq!(delivered(held).await, Delivery::Sent);
    assert!(matches!(
        bridge.handle(message(5, "/status")).await,
        Handled::Answered
    ));
    // The same /stop again, after a later message started a turn: it does
    // not reach that turn.
    let later = bridge.handle(message(6, "hold")).await;
    assert!(matches!(
        bridge.handle(message(4, "/stop")).await,
        Handled::Answered
    ));
    llm.release.add_permits(1);
    assert_eq!(delivered(later).await, Delivery::Sent);
    assert_eq!(
        channel.take(),
        [
            (ME, "A turn is running.".into(), Some(3)),
            (ME, "Stopped.".into(), Some(2)),
            (ME, "Nothing is running.".into(), Some(5)),
            (ME, STOP_TOO_LATE.into(), Some(4)),
            (ME, "reply: hold".into(), Some(6)),
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
        (
            vec![ME],
            vec![binding(ME, SOMEONE, &chat)],
            "is not allowed",
        ),
        (
            vec![777_000],
            vec![binding(ME, 777_000, &chat)],
            "speaks for many",
        ),
        (vec![ME], vec![elsewhere], "another channel"),
        (
            vec![ME],
            vec![binding(ME, ME, &chat), binding(ME, ME, &second)],
            "bound twice",
        ),
        (
            vec![ME, SOMEONE],
            vec![binding(ME, ME, &chat), binding(SOMEONE, SOMEONE, &chat)],
            "bound to two chats",
        ),
    ];
    for (allowed, bindings, why) in refused {
        let channel = Arc::new(FakeChannel::default());
        let error = ServeImBridge::new(turns(), channel, allowed, bindings, TTL).err();
        assert!(
            error.as_deref().unwrap_or_default().contains(why),
            "{error:?}"
        );
    }

    let (bridge, channel) = bridge(&fx, &chat);
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
    // The chat was deleted after it was bound: the start is refused.
    assert!(
        delete_session_as_admin(&fx.state, chat.session_id())
            .await
            .is_success()
    );
    assert!(matches!(
        bridge.handle(message(12, "hi")).await,
        Handled::Refused(Refusal::NotStarted(_))
    ));
    replies.push(ME);
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
    let (bridge, channel) = bridge(&fx, &chat);

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
        let (bridge, channel) = bridge(&fx, &chat);
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
    let (bridge, channel) = bridge(&fx, &chat);
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
    // The /stop that stopped message 4's turn, delivered again: the turn
    // a later message started keeps running.
    let later = bridge.handle(message(6, "hold")).await;
    assert!(matches!(
        bridge.handle(message(5, "/stop")).await,
        Handled::Answered
    ));
    llm.release.add_permits(1);
    assert_eq!(delivered(later).await, Delivery::Sent);
    assert_eq!(
        channel.take(),
        [
            (ME, STOP_TOO_LATE.into(), Some(5)),
            (ME, "reply: hold".into(), Some(6)),
        ]
    );
    // Nothing ran twice; the stopped turn may end before it reaches the
    // model.
    let seen = prompts(&llm);
    assert_eq!(seen[..2], ["before", "lost"]);
    assert!(matches!(seen[2..].len(), 1 | 2) && seen[2..].iter().all(|p| p == "hold"));
}
