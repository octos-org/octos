//! T-0663: the Telegram transport against a local fake Bot API server, a
//! real serve runtime and an existing chat. The fake is not Telegram: no
//! real bot, token or network is involved.
use std::collections::{HashMap, VecDeque};

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

/// A local Bot API server for one bot: the updates it holds, what it was
/// asked, and answers it gives before its own.
#[derive(Default)]
struct FakeTelegram {
    pending: StdMutex<Vec<Value>>,
    calls: StdMutex<Vec<(String, Value)>>,
    answers: StdMutex<HashMap<String, VecDeque<(StatusCode, &'static str)>>>,
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

    /// Answer the next call of `method` so, whatever it asks.
    fn answer_next(&self, method: &str, status: StatusCode, body: &'static str) {
        let mut answers = self.answers.lock().unwrap();
        answers
            .entry(method.into())
            .or_default()
            .push_back((status, body));
    }

    fn push(&self, update: Value) {
        self.pending.lock().unwrap().push(update);
    }

    /// Each `sendMessage`: chat, text, the message replied to, and HTML.
    fn sends(&self) -> Vec<(i64, String, Option<i64>, bool)> {
        let calls = self.calls.lock().unwrap();
        let sends = calls.iter().filter(|(method, _)| method == "sendMessage");
        sends
            .map(|(_, body)| {
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
        let polls = calls.iter().filter(|(method, _)| method == "getUpdates");
        polls
            .map(|(_, body)| body["offset"].as_i64().unwrap())
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
    fake.calls
        .lock()
        .unwrap()
        .push((method.clone(), body.clone()));
    let next = fake
        .answers
        .lock()
        .unwrap()
        .get_mut(&method)
        .and_then(VecDeque::pop_front);
    if let Some(answer) = next {
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

async fn until<T>(what: &str, mut ready: impl FnMut() -> Option<T>) -> T {
    for _ in 0..1000 {
        if let Some(done) = ready() {
            return done;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{what} did not happen");
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
    fake.answer_next("getUpdates", StatusCode::BAD_GATEWAY, "Bad Gateway");
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
    let sends = until("both replies", || {
        Some(fake.sends()).filter(|s| s.len() == 2)
    })
    .await;
    assert!(sends.contains(&(ME, "reply: hello".into(), Some(50), true)));
    let refused = "This chat is not linked to octos.".to_owned();
    assert!(sends.contains(&(GROUP, refused, Some(51), true)));
    // Both are told done by asking from the offset after them.
    let polled_since = || {
        let polls = fake.polls();
        let told = polls.iter().position(|offset| *offset == 52)?;
        (polls.len() > told + 1).then_some(())
    };
    until("a poll after telling", polled_since).await;
    // After a quiet week Telegram numbers the next update at random, here
    // below the last offset: it still reaches the bridge before it is told.
    fake.push(update(7, ME, "private", "a week later"));
    let sends = until("the reply", || Some(fake.sends()).filter(|s| s.len() == 3)).await;
    assert_eq!(sends[2], (ME, "reply: a week later".into(), Some(7), true));
    until("asking from 8", || fake.asked_from(8).then_some(())).await;
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
    until("asking from 53", || fake.asked_from(53).then_some(())).await;
    let sends = until("the reply", || Some(fake.sends()).filter(|s| !s.is_empty())).await;
    assert_eq!(sends, [(ME, "reply: again".into(), Some(52), true)]);
    let prompts = llm.prompts.lock().unwrap().clone();
    assert_eq!(prompts, ["hello", "a week later", "again"]);
}

#[tokio::test]
async fn a_send_telegram_did_not_answer_is_not_repeated_and_no_error_shows_the_token() {
    let (fake, url) = FakeTelegram::start().await;
    let id = ImChannelId {
        platform: "telegram".into(),
        bot: 900,
    };
    let bot = TelegramBot::new(id, TelegramBotApi::new(&url, TOKEN));
    // Telegram took it, but its answer was lost.
    fake.answer_next("sendMessage", StatusCode::BAD_GATEWAY, "Bad Gateway");
    let lost = bot.send(ME, "hi", Some(7)).await.unwrap_err().to_string();
    assert_eq!(fake.sends(), [(ME, "hi".into(), Some(7), true)]);

    // A piece refused for its HTML goes again as text; a long reply goes in
    // pieces, only the first a reply.
    let bad_html =
        r#"{"ok":false,"error_code":400,"description":"Bad Request: can't parse entities"}"#;
    fake.answer_next("sendMessage", StatusCode::BAD_REQUEST, bad_html);
    bot.send(ME, &"word ".repeat(1000), Some(8)).await.unwrap();
    let sent: Vec<_> = fake.sends()[1..].iter().map(|s| (s.2, s.3)).collect();
    assert_eq!(sent, [(Some(8), true), (Some(8), false), (None, true)]);

    // Neither a refusal nor a request with no answer names the token.
    let unauthorized = r#"{"ok":false,"error_code":401,"description":"Unauthorized"}"#;
    fake.answer_next("getUpdates", StatusCode::UNAUTHORIZED, unauthorized);
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
