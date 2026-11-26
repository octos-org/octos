//! T-0663, T-0665: the Telegram transport against a local fake Bot API
//! server, a real serve runtime and an existing chat. The fake is not
//! Telegram: no real bot, token or network is involved.
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, routing::post};
use octos_bus::telegram_api::Update;

use super::super::external_turns::tests::{restart, scripted, serve, unique};
use super::super::im_bridge::tests::ME;
use super::super::im_config::start;
use super::super::tests::test_support::*;
use super::*;
use crate::config::{ServeImBinding, ServeImChannel, ServeImPlatform};

const TOKEN: &str = "900:fake-secret";
const GROUP: i64 = -100_200;
/// Telegram's answer to a bot sending too fast.
const SLOW_DOWN_1S: &str = r#"{"ok":false,"error_code":429,"description":"Too Many Requests: retry after 1","parameters":{"retry_after":1}}"#;
const SLOW_DOWN_2S: &str = r#"{"ok":false,"error_code":429,"description":"Too Many Requests: retry after 2","parameters":{"retry_after":2}}"#;
const SLOW_DOWN_30S: &str = r#"{"ok":false,"error_code":429,"description":"Too Many Requests: retry after 30","parameters":{"retry_after":30}}"#;
const SLOW_DOWN_1H: &str = r#"{"ok":false,"error_code":429,"description":"Too Many Requests: retry after 3600","parameters":{"retry_after":3600}}"#;

/// An answer the fake gives instead of its own.
type Answer = (StatusCode, &'static str);

/// A local Bot API server for one bot: the updates it holds, what it was
/// asked and when, answers it gives before its own (`None`: its own), and
/// the most `getUpdates` it had at once.
#[derive(Default)]
struct FakeTelegram {
    pending: StdMutex<Vec<Value>>,
    calls: StdMutex<Vec<(String, Value, Instant)>>,
    answers: StdMutex<HashMap<String, VecDeque<Option<Answer>>>>,
    polling: AtomicUsize,
    most_polling: AtomicUsize,
}

impl FakeTelegram {
    async fn start() -> (Arc<Self>, String) {
        let fake = Arc::new(Self::default());
        let app = Router::new()
            .route("/{bot}/{method}", post(answer))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (fake, url)
    }

    /// Answer the next call of `method` so, whatever it asks; `None`
    /// answers it as Telegram would.
    fn answer_next(&self, method: &str, answer: Option<Answer>) {
        let mut answers = self.answers.lock().unwrap();
        answers.entry(method.into()).or_default().push_back(answer);
    }

    fn times(&self, method: &str) -> Vec<Instant> {
        let calls = self.calls.lock().unwrap();
        let calls = calls.iter().filter(|(called, ..)| called == method);
        calls.map(|(.., at)| *at).collect()
    }

    fn push(&self, update: Value) {
        self.pending.lock().unwrap().push(update);
    }

    /// Each `sendMessage`: chat, text, the message replied to, and HTML.
    fn sends(&self) -> Vec<(i64, String, Option<i64>, bool)> {
        let calls = self.calls.lock().unwrap();
        let sends = calls.iter().filter(|(method, ..)| method == "sendMessage");
        sends
            .map(|(_, body, _)| {
                let text = body["text"].as_str().unwrap().to_owned();
                let reply_to = body["reply_parameters"]["message_id"].as_i64();
                (
                    body["chat_id"].as_i64().unwrap(),
                    text,
                    reply_to,
                    body["parse_mode"] == "HTML",
                )
            })
            .collect()
    }

    fn asked_from(&self, offset: i64) -> bool {
        self.polls().contains(&offset)
    }

    /// The offset of each `getUpdates`, in order.
    fn polls(&self) -> Vec<i64> {
        let calls = self.calls.lock().unwrap();
        let polls = calls.iter().filter(|(method, ..)| method == "getUpdates");
        polls
            .map(|(_, body, _)| body["offset"].as_i64().unwrap())
            .collect()
    }
}

async fn answer(
    State(fake): State<Arc<FakeTelegram>>,
    UrlPath((bot, method)): UrlPath<(String, String)>,
    Json(body): Json<Value>,
) -> Response {
    if bot != format!("bot{TOKEN}") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let _polling = Polling::start(&fake, &method);
    fake.calls
        .lock()
        .unwrap()
        .push((method.clone(), body.clone(), Instant::now()));
    let next = fake
        .answers
        .lock()
        .unwrap()
        .get_mut(&method)
        .and_then(VecDeque::pop_front);
    if let Some(Some(answer)) = next {
        return answer.into_response();
    }
    let result = if method == "getUpdates" {
        // Asking from an offset tells Telegram every update before it is done.
        let offset = body["offset"].as_i64().unwrap();
        let pending = {
            let mut pending = fake.pending.lock().unwrap();
            pending.retain(|update| update["update_id"].as_i64().unwrap() >= offset);
            pending.clone()
        };
        if pending.is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        json!(pending)
    } else {
        json!({ "message_id": 1 })
    };
    Json(json!({ "ok": true, "result": result })).into_response()
}

/// A `getUpdates` the fake is answering.
struct Polling<'a>(Option<&'a FakeTelegram>);

impl<'a> Polling<'a> {
    fn start(fake: &'a FakeTelegram, method: &str) -> Self {
        let fake = (method == "getUpdates").then_some(fake);
        if let Some(fake) = fake {
            let now = fake.polling.fetch_add(1, Ordering::SeqCst) + 1;
            fake.most_polling.fetch_max(now, Ordering::SeqCst);
        }
        Self(fake)
    }
}

impl Drop for Polling<'_> {
    fn drop(&mut self) {
        if let Some(fake) = self.0 {
            fake.polling.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

fn update(id: i64, chat: i64, kind: &str, text: &str) -> Value {
    json!({ "update_id": id, "message": {
        "message_id": id,
        "date": Utc::now().timestamp(),
        "chat": { "id": chat, "type": kind },
        "from": { "id": ME, "is_bot": false, "first_name": "Me" },
        "text": text,
    }})
}

fn env(name: &str) -> Option<String> {
    (name == "TG").then(|| TOKEN.to_owned())
}

/// My direct chat with bot 900, bound to `session`.
fn entries(session: &SessionKey) -> Vec<ServeImChannel> {
    let binding = ServeImBinding {
        chat: ME,
        sender: ME,
        profile: CHAT_PROFILE.into(),
        session: session.0.clone(),
    };
    vec![ServeImChannel {
        platform: ServeImPlatform::Telegram,
        token_env: "TG".into(),
        allowed_senders: vec![ME],
        ttl_secs: 600,
        bindings: vec![binding],
    }]
}

#[test]
fn a_telegram_message_reads_as_the_platform_tells_it() {
    let read = |message: Value| {
        let update = json!({ "update_id": 1, "message": message });
        let update: Update = serde_json::from_value(update).unwrap();
        im_message(&update.message.unwrap())
    };
    let base = json!({
        "message_id": 5,
        "date": 1_700_000_000,
        "chat": { "id": ME, "type": "private", "first_name": "Me" },
        "from": { "id": ME, "is_bot": false, "first_name": "Me", "language_code": "en" },
        "text": "hi",
        "entities": [],
    });
    let mine = read(base.clone());
    let fields = (mine.chat, mine.direct, mine.sender, mine.message_id);
    assert_eq!(fields, (ME, true, ImSender::Person(ME), Some(5)));
    assert_eq!((mine.text.as_str(), mine.attachments), ("hi", 0));
    assert_eq!(mine.sent_at, DateTime::from_timestamp(1_700_000_000, 0));

    let with = |edit: fn(&mut Value)| {
        let mut message = base.clone();
        edit(&mut message);
        read(message)
    };
    assert!(!with(|m| m["chat"] = json!({ "id": GROUP, "type": "supergroup" })).direct);
    assert_eq!(
        with(|m| m["from"]["is_bot"] = json!(true)).sender,
        ImSender::Bot(ME)
    );
    // An anonymous admin writes as GroupAnonymousBot, in the group's name.
    let anonymous = with(|m| {
        m["from"] = json!({ "id": 1_087_968_824, "is_bot": true, "first_name": "Group" });
        m["sender_chat"] = json!({ "id": GROUP, "type": "supergroup" });
    });
    assert_eq!(anonymous.sender, ImSender::Chat(GROUP));
    // A photo is an attachment, whatever its caption says.
    let photo = with(|m| {
        m.as_object_mut().unwrap().remove("text");
        m["caption"] = json!("look");
        m["photo"] = json!([{ "file_id": "x", "width": 1, "height": 1 }]);
    });
    assert_eq!((photo.text.as_str(), photo.attachments), ("look", 1));
}

#[tokio::test]
async fn a_direct_message_runs_once_across_a_redelivery_and_a_restart() {
    let (dir, _hidden) = crate::runtime::chat_workspace::tempdir_with_hidden_guard();
    let llm = scripted();
    let fx = serve(dir.path(), llm.clone()).await;
    let chat = unique("im-telegram");
    fx.chat(&chat).await;
    let owners = tempfile::tempdir().unwrap();
    let (fake, url) = FakeTelegram::start().await;
    // The first poll gets no answer; the transport asks again.
    fake.answer_next("getUpdates", Some((StatusCode::BAD_GATEWAY, "Bad Gateway")));
    fake.push(update(50, ME, "private", "hello"));
    fake.push(update(51, GROUP, "group", "hello all"));
    let im = start(
        &fx.state,
        &entries(&chat),
        env,
        owners.path(),
        "serve",
        &url,
    )
    .await;
    let im = im.unwrap();
    let sends = until_some(|| Some(fake.sends()).filter(|s| s.len() == 2)).await;
    assert!(sends.contains(&(ME, "reply: hello".into(), Some(50), true)));
    let refused = "This chat is not linked to octos.".to_owned();
    assert!(sends.contains(&(GROUP, refused, Some(51), true)));
    // Both are told done by asking from the offset after them.
    let polled_since = || {
        let polls = fake.polls();
        let told = polls.iter().position(|offset| *offset == 52)?;
        (polls.len() > told + 1).then_some(())
    };
    until_some(polled_since).await;
    // After a quiet week Telegram numbers the next update at random, here
    // below the last offset: it still reaches the bridge before it is told.
    // The unlinked group, told once, is not told again.
    fake.push(update(6, GROUP, "group", "hello again"));
    fake.push(update(7, ME, "private", "a week later"));
    let sends = until_some(|| Some(fake.sends()).filter(|s| s.len() == 3)).await;
    assert_eq!(sends[2], (ME, "reply: a week later".into(), Some(7), true));
    until_some(|| fake.asked_from(8).then_some(())).await;
    drop(im);
    drop(fx);

    // After a restart Telegram sends again an update it was never told about
    // (serve stopped before its next poll), and a new one: only the new one
    // runs, and only it is answered.
    let (fake, url) = FakeTelegram::start().await;
    fake.push(update(50, ME, "private", "hello"));
    fake.push(update(52, ME, "private", "again"));
    let fx = restart(dir.path(), llm.clone()).await;
    let im = start(
        &fx.state,
        &entries(&chat),
        env,
        owners.path(),
        "serve",
        &url,
    )
    .await;
    let _im = im.unwrap();
    until_some(|| fake.asked_from(53).then_some(())).await;
    let sends = until_some(|| Some(fake.sends()).filter(|s| !s.is_empty())).await;
    assert_eq!(sends, [(ME, "reply: again".into(), Some(52), true)]);
    let prompts = llm.prompts.lock().unwrap().clone();
    assert_eq!(prompts, ["hello", "a week later", "again"]);
}

#[tokio::test]
async fn a_rate_limited_refusal_does_not_hold_up_the_next_stop() {
    let (dir, _hidden) = crate::runtime::chat_workspace::tempdir_with_hidden_guard();
    let fx = serve(dir.path(), scripted()).await;
    let chat = unique("im-telegram-stop");
    fx.chat(&chat).await;
    let owners = tempfile::tempdir().unwrap();
    let (fake, url) = FakeTelegram::start().await;
    // The unlinked group's refusal is told to wait 30 s, longer than this
    // test waits; the /stop after it in the same batch is answered anyway.
    let slow_down = (StatusCode::TOO_MANY_REQUESTS, SLOW_DOWN_30S);
    fake.answer_next("sendMessage", Some(slow_down));
    fake.push(update(70, GROUP, "group", "hello all"));
    fake.push(update(71, ME, "private", "/stop"));
    let im = start(
        &fx.state,
        &entries(&chat),
        env,
        owners.path(),
        "serve",
        &url,
    )
    .await;
    let _im = im.unwrap();
    let sends = until_some(|| Some(fake.sends()).filter(|s| s.len() == 2)).await;
    let answered: Vec<_> = sends.iter().map(|s| (s.0, s.2)).collect();
    assert_eq!(answered, [(GROUP, Some(70)), (ME, Some(71))]);
}

#[tokio::test]
async fn a_send_telegram_did_not_answer_is_not_repeated_and_no_error_shows_the_token() {
    let (fake, url) = FakeTelegram::start().await;
    let id = ImChannelId {
        platform: "telegram".into(),
        bot: 900,
    };
    let bot = TelegramBot::new(id, TelegramBotApi::new(&url, TOKEN));
    let sent_since =
        |from: usize| -> Vec<_> { fake.sends()[from..].iter().map(|s| (s.2, s.3)).collect() };
    // Telegram took it, but its answer was lost.
    fake.answer_next(
        "sendMessage",
        Some((StatusCode::BAD_GATEWAY, "Bad Gateway")),
    );
    let lost = bot.send(ME, "hi", Some(7)).await.unwrap_err().to_string();
    assert_eq!(fake.sends(), [(ME, "hi".into(), Some(7), true)]);

    // A long reply goes in pieces, only the first a reply. One Telegram
    // refused for sending too fast goes again after the wait it asked for,
    // and the pieces after it still go.
    fake.answer_next("sendMessage", None);
    fake.answer_next(
        "sendMessage",
        Some((StatusCode::TOO_MANY_REQUESTS, SLOW_DOWN_1S)),
    );
    bot.send(ME, &"word ".repeat(1000), Some(8)).await.unwrap();
    assert_eq!(sent_since(1), [(Some(8), true), (None, true), (None, true)]);
    let times = fake.times("sendMessage");
    assert!(times[3] - times[2] >= Duration::from_secs(1));
    // A piece refused for its HTML goes again as text. Told to wait longer
    // than a send may, it is not repeated.
    let bad_html =
        r#"{"ok":false,"error_code":400,"description":"Bad Request: can't parse entities"}"#;
    fake.answer_next("sendMessage", Some((StatusCode::BAD_REQUEST, bad_html)));
    bot.send(ME, "hi", Some(9)).await.unwrap();
    fake.answer_next(
        "sendMessage",
        Some((StatusCode::TOO_MANY_REQUESTS, SLOW_DOWN_1H)),
    );
    assert!(bot.send(ME, "hi", Some(10)).await.is_err());
    let sent = sent_since(4);
    assert_eq!(sent, [(Some(9), true), (Some(9), false), (Some(10), true)]);

    // Neither a refusal nor a request with no answer names the token.
    let unauthorized = r#"{"ok":false,"error_code":401,"description":"Unauthorized"}"#;
    fake.answer_next("getUpdates", Some((StatusCode::UNAUTHORIZED, unauthorized)));
    let api = TelegramBotApi::new(&url, TOKEN);
    let refused = api.get_updates(0, Duration::ZERO).await.unwrap_err();
    assert!(matches!(refused, ApiError::Refused { code: 401, .. }));
    let closed = {
        let nothing_listens = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", nothing_listens.local_addr().unwrap())
    };
    let nobody = TelegramBotApi::new(&closed, TOKEN);
    let unanswered = nobody.get_updates(0, Duration::ZERO).await.unwrap_err();
    assert!(matches!(unanswered, ApiError::Unanswered(_)));
    for error in [
        lost,
        refused.to_string(),
        unanswered.to_string(),
        format!("{api:?}"),
    ] {
        assert!(
            !error.contains("fake-secret") && !error.contains("/bot"),
            "{error}"
        );
    }
}

#[tokio::test]
async fn neither_an_unreadable_update_nor_a_panic_holds_up_the_bot_and_it_is_polled_once() {
    let (fake, url) = FakeTelegram::start().await;
    let id = ImChannelId {
        platform: "telegram".into(),
        bot: 900,
    };
    let bot = TelegramBot::new(id, TelegramBotApi::new(&url, TOKEN));
    // Told to slow down, the poller waits as long as Telegram asked, which
    // is longer than its own backoff.
    fake.answer_next(
        "getUpdates",
        Some((StatusCode::TOO_MANY_REQUESTS, SLOW_DOWN_2S)),
    );
    fake.push(update(60, ME, "private", "one"));
    // No chat: it does not read as a message.
    fake.push(json!({ "update_id": 61, "message": { "message_id": 61, "date": 0, "text": "?" } }));
    fake.push(update(62, ME, "private", "panic"));
    fake.push(update(63, ME, "private", "two"));
    let seen = Arc::new(StdMutex::new(Vec::<String>::new()));
    let handled = seen.clone();
    let poller = tokio::spawn(async move {
        let handle = |message: ImMessage| {
            let handled = handled.clone();
            async move {
                handled.lock().unwrap().push(message.text.clone());
                assert_ne!(message.text, "panic", "a bug that panics on one message");
            }
        };
        poll_with(&bot, handle, |_| async {}).await
    });
    until_some(|| fake.asked_from(64).then_some(())).await;
    assert_eq!(*seen.lock().unwrap(), ["one", "panic", "two"]);
    let polls = fake.times("getUpdates");
    assert!(polls[1] - polls[0] >= Duration::from_secs(2));
    // The next batch is handled too, and the bot never had two polls.
    fake.push(update(65, ME, "private", "three"));
    until_some(|| fake.asked_from(66).then_some(())).await;
    assert_eq!(
        seen.lock().unwrap().last().map(String::as_str),
        Some("three")
    );
    assert_eq!(fake.most_polling.load(Ordering::SeqCst), 1);
    poller.abort();
}

/// T-0690: a button press, the keyboard, the ACK and the invalidating edit.
#[tokio::test]
async fn approval_buttons_press_and_edit_through_the_bot_api() {
    let press = json!({ "update_id": 9, "callback_query": { "id": "cb-9", "from": { "id": ME, "is_bot": false }, "data": "imp:0123456789abcdef:a", "message": { "message_id": 42, "date": 1, "chat": { "id": ME, "type": "private" } } } });
    let update: Update = serde_json::from_value(press).unwrap();
    let callback = im_callback(update.callback_query.as_ref().unwrap()).unwrap();
    let got = (callback.chat, callback.message_id, callback.callback_id.as_str(), callback.data.as_str());
    assert_eq!(got, (ME, Some(42), "cb-9", "imp:0123456789abcdef:a"));

    let (fake, url) = FakeTelegram::start().await;
    let bot = TelegramBotApi::new(&url, TOKEN);
    let keyboard = [vec![("Approve".to_owned(), "imp:x:a".to_owned())]];
    let id = bot.send_message_keyboard(ME, "Approval", false, None, &keyboard).await.unwrap();
    bot.answer_callback_query("cb-9", Some("Approved."), false).await.unwrap();
    bot.edit_message_text(ME, id, "Approved.", false).await.unwrap();
    let calls = fake.calls.lock().unwrap();
    let called = |method: &str| &calls.iter().find(|(m, ..)| m == method).unwrap().1;
    assert_eq!(called("sendMessage")["reply_markup"]["inline_keyboard"][0][0]["callback_data"], json!("imp:x:a"));
    assert_eq!(called("answerCallbackQuery")["callback_query_id"], json!("cb-9"));
    assert_eq!(called("editMessageText")["reply_markup"]["inline_keyboard"], json!([]));
}
