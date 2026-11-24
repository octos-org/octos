//! T-0654: `serve.im` against a real serve runtime and existing chats: what
//! the config may say, who owns a bot, and a binding taken out of it.
use super::super::external_turns::tests::{restart, scripted, serve, unique};
use super::super::im_bridge::tests::{FakeChannel, ME, SOMEONE, message};
use super::super::im_bridge::{Delivery, Handled, ImMessage, ImSender, Refusal};
use super::super::tests::test_support::*;
use super::*;
use crate::config::Config;

const BOT: i64 = 900;

/// The test's environment: `TG` holds bot 900's token, `TG_BAD` no token.
fn env(name: &str) -> Option<String> {
    match name {
        "TG" => Some(format!("{BOT}:secret")),
        "TG_BAD" => Some("secret".into()),
        _ => None,
    }
}

/// `serve.im` with one bot binding `who`'s chat to `session`, changed by
/// `edit`, as serve would parse it.
fn config(
    who: i64,
    session: &SessionKey,
    edit: impl FnOnce(&mut Value),
) -> Result<Vec<ServeImChannel>, String> {
    let mut entry = json!({
        "type": "telegram",
        "token_env": "TG",
        "allowed_senders": [who],
        "bindings": [{ "chat": who, "sender": who, "profile": CHAT_PROFILE, "session": session.0 }],
    });
    edit(&mut entry);
    let config: Config = serde_json::from_value(json!({ "serve": { "im": [entry] } }))
        .map_err(|error| error.to_string())?;
    Ok(config.serve.unwrap().im)
}

async fn assemble_on(
    state: &Arc<AppState>,
    entries: &[ServeImChannel],
    owners: &Path,
    holder: &str,
    channel: &Arc<FakeChannel>,
) -> Result<ServeIm, String> {
    assemble(state, entries, env, owners, holder, |_, _| channel.clone()).await
}

/// Changes the first binding's `field` to `value`.
fn binding(field: &'static str, value: Value) -> impl FnOnce(&mut Value) {
    move |entry| entry["bindings"][0][field] = value
}

fn from(who: i64, id: i64, text: &str) -> ImMessage {
    ImMessage {
        chat: who,
        sender: ImSender::Person(who),
        ..message(id, text)
    }
}

#[tokio::test]
async fn serve_im_refuses_any_entry_it_cannot_bind_exactly() {
    // Without the section nothing changes, and nothing is written back.
    let plain: Config = serde_json::from_value(json!({})).unwrap();
    assert_eq!(plain.serve, None);
    assert_eq!(serde_json::to_value(&plain).unwrap().get("serve"), None);

    let (dir, _hidden) = crate::runtime::chat_workspace::tempdir_with_hidden_guard();
    let fx = serve(dir.path(), scripted()).await;
    let chat = unique("im-config");
    fx.chat(&chat).await;
    let code = SessionKey::with_profile(CHAT_PROFILE, "api", "code-session");
    let owners = tempfile::tempdir().unwrap();
    let channel = Arc::new(FakeChannel::default());
    let cases: Vec<(&str, Result<Vec<ServeImChannel>, String>)> = vec![
        // A gateway channel's default-profile routes mean nothing here.
        (
            "unknown field `chat_profile_map`",
            config(ME, &chat, |e| e["chat_profile_map"] = json!({ "1": "x" })),
        ),
        (
            "unknown variant `discord`",
            config(ME, &chat, |e| e["type"] = json!("discord")),
        ),
        (
            "missing field `bindings`",
            config(ME, &chat, |e| {
                e.as_object_mut().unwrap().remove("bindings");
            }),
        ),
        (
            "missing field `profile`",
            config(ME, &chat, |e| {
                e["bindings"][0].as_object_mut().unwrap().remove("profile");
            }),
        ),
        (
            "serve.im[0]: TG_UNSET is not set",
            config(ME, &chat, |e| e["token_env"] = json!("TG_UNSET")),
        ),
        (
            "TG_BAD does not hold a Telegram bot token",
            config(ME, &chat, |e| e["token_env"] = json!("TG_BAD")),
        ),
        (
            "no bindings",
            config(ME, &chat, |e| e["bindings"] = json!([])),
        ),
        (
            "no allowed senders",
            config(ME, &chat, |e| e["allowed_senders"] = json!([])),
        ),
        (
            "profile 'nobody' is not configured",
            config(ME, &chat, binding("profile", json!("nobody"))),
        ),
        (
            "not named as a session of the bound profile",
            config(ME, &chat, binding("session", json!("nobody:api:x"))),
        ),
        (
            "Code sessions cannot be bound",
            config(ME, &chat, binding("session", json!(code.0))),
        ),
        (
            "telegram bot 900 is listed twice",
            config(ME, &chat, |_| {}).map(|entries| [entries.clone(), entries].concat()),
        ),
    ];
    for (why, entries) in cases {
        let refused = match entries {
            Err(parse) => parse,
            Ok(entries) => assemble_on(&fx.state, &entries, owners.path(), "serve", &channel)
                .await
                .err()
                .unwrap_or_else(|| panic!("accepted where {why}")),
        };
        assert!(refused.contains(why), "{why}: {refused}");
    }
    assert!(channel.take().is_empty());
}

#[tokio::test]
async fn a_bot_has_one_owner_serve_or_one_gateway() {
    let (dir, _hidden) = crate::runtime::chat_workspace::tempdir_with_hidden_guard();
    let fx = serve(dir.path(), scripted()).await;
    let chat = unique("im-owner");
    fx.chat(&chat).await;
    let owners = tempfile::tempdir().unwrap();
    let owners = owners.path();
    let channel = Arc::new(FakeChannel::default());
    let entries = config(ME, &chat, |_| {}).unwrap();
    let ours = assemble_on(&fx.state, &entries, owners, "serve A", &channel).await;
    let ours = ours.unwrap();

    // Another serve of this user, whatever its data directory, and any
    // gateway are refused the bot, by its id and not by the setting that
    // names it.
    let again = assemble_on(&fx.state, &entries, owners, "serve B", &channel).await;
    let again = again.err().unwrap();
    assert!(again.contains("telegram bot 900 already has an owner (serve A, pid"));
    let token = format!("{BOT}:another-secret");
    let gateway = BotOwner::claim_for_gateway(&token, Some(owners), "gateway").unwrap_err();
    assert!(gateway.contains("(serve A, pid"), "{gateway}");

    // A gateway's other bots, and tokens with no bot id, run as before.
    let other = BotOwner::claim_for_gateway("901:secret", Some(owners), "gateway");
    assert!(other.unwrap().is_some());
    for odd in ["", "secret", "900", "900:", "-1:secret", "x:secret"] {
        let claimed = BotOwner::claim_for_gateway(odd, Some(owners), "gateway");
        assert!(claimed.unwrap().is_none(), "{odd}");
    }

    // A lock it cannot take refuses the gateway too, rather than leave two
    // gateways polling one bot.
    let not_a_dir = owners.join("not-a-dir");
    std::fs::write(&not_a_dir, "").unwrap();
    for dir in [Some(not_a_dir.as_path()), None] {
        let unusable = BotOwner::claim_for_gateway("902:secret", dir, "gateway").unwrap_err();
        assert!(
            unusable.starts_with("telegram channel refused: "),
            "{unusable}"
        );
    }

    // Once serve lets go, a gateway may have it.
    drop(ours);
    let handed = BotOwner::claim_for_gateway(&token, Some(owners), "gateway");
    assert!(handed.unwrap().is_some());
}

#[tokio::test]
async fn a_binding_taken_out_of_the_config_is_refused_from_the_next_start() {
    let (dir, _hidden) = crate::runtime::chat_workspace::tempdir_with_hidden_guard();
    let llm = scripted();
    let fx = serve(dir.path(), llm.clone()).await;
    let mine = unique("im-mine");
    fx.chat(&mine).await;
    let theirs = unique("im-theirs");
    fx.chat(&theirs).await;
    let owners = tempfile::tempdir().unwrap();
    let channel = Arc::new(FakeChannel::default());
    let entries = config(ME, &mine, |_| {}).unwrap();
    let im = assemble_on(&fx.state, &entries, owners.path(), "serve", &channel).await;
    let Handled::Started(delivery) = im.unwrap().bridges[0].handle(from(ME, 1, "hello")).await
    else {
        panic!("my message did not run");
    };
    assert_eq!(delivery.await.unwrap(), Delivery::Sent);
    assert_eq!(channel.take(), [(ME, "reply: hello".into(), Some(1))]);
    drop(fx);

    // The operator binds their chat instead of mine and restarts serve: my
    // next message and a redelivery of my last are refused, and none runs.
    let fx = restart(dir.path(), llm.clone()).await;
    let entries = config(SOMEONE, &theirs, |_| {}).unwrap();
    let im = assemble_on(&fx.state, &entries, owners.path(), "serve", &channel).await;
    let im = im.unwrap();
    for (id, text) in [(2, "again"), (1, "hello")] {
        let handled = im.bridges[0].handle(from(ME, id, text)).await;
        assert!(matches!(handled, Handled::Refused(Refusal::UnknownChat)));
    }
    assert_eq!(llm.prompts.lock().unwrap().len(), 1);
}
