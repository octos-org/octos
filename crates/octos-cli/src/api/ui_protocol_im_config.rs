//! `serve.im` (T-0654, `docs/IM_REMOTE_CONTROL.md` §7, §11): the bridges a
//! serve's config asks for, made before serve starts a gateway or takes a
//! request.
//!
//! Each entry is one bot. Serve takes the bot first (its owner lock, by the
//! bot's id), so no gateway, of this serve or of another, consumes it too.
//! Then every binding must name an existing chat of a profile this serve
//! serves, and the bridge must accept the whole entry. Anything wrong stops
//! serve with a message that names the entry: no default profile, no
//! allow-all, no partial start. The config is read once, so a binding
//! removed from it, a revocation included, is gone from the next start on.
//!
//! The Telegram transport that feeds a bridge is the next cut. Until then a
//! bridge made here receives nothing, and serve only keeps its bot from
//! every gateway.

use std::collections::HashSet;

use crate::bot_owner::{self, BotOwner, HANDOFF};
use crate::config::{ServeImChannel, ServeImPlatform};

use super::external_turns::{BoundChat, BoundProfile, ExternalTurns};
use super::im_bridge::{ImBinding, ImChannel, ImChannelId, ServeImBridge};
use super::*;

/// What `serve.im` made, kept while serve runs.
pub(crate) struct ServeIm {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) bridges: Vec<Arc<ServeImBridge>>,
    _owners: Vec<BotOwner>,
}

/// `serve.im` for this serve: tokens from its environment, locks in this
/// user's directory, and no transport connected yet.
pub(crate) async fn start_serve_im(
    state: &Arc<AppState>,
    entries: &[ServeImChannel],
    data_dir: &Path,
) -> eyre::Result<ServeIm> {
    let dir = bot_owner::default_dir()
        .ok_or_else(|| eyre::eyre!("serve.im: no home directory to keep bot locks in"))?;
    let holder = format!("octos serve, data dir {}", data_dir.display());
    let im = assemble(
        state,
        entries,
        |name| std::env::var(name).ok(),
        &dir,
        &holder,
        |id| Arc::new(Unconnected(id)),
    )
    .await
    .map_err(|why| eyre::eyre!(why))?;
    warn!(
        bots = im.bridges.len(),
        "serve.im: bots taken and bindings checked; this build receives none of their messages yet"
    );
    Ok(im)
}

/// Make `entries`' bridges: each bot's token read through `env`, its lock
/// taken in `owners_dir` for `holder`, its replies sent through `channel`.
pub(crate) async fn assemble(
    state: &Arc<AppState>,
    entries: &[ServeImChannel],
    env: impl Fn(&str) -> Option<String>,
    owners_dir: &Path,
    holder: &str,
    channel: impl Fn(ImChannelId) -> Arc<dyn ImChannel>,
) -> Result<ServeIm, String> {
    let ledger = event_ledger(state).await;
    let mut im = ServeIm {
        bridges: Vec::new(),
        _owners: Vec::new(),
    };
    let mut bots = HashSet::new();
    for (index, entry) in entries.iter().enumerate() {
        let refuse = |why: String| format!("serve.im[{index}]: {why}");
        let platform = match entry.platform {
            ServeImPlatform::Telegram => "telegram",
        };
        let name = &entry.token_env;
        let token = env(name).ok_or_else(|| refuse(format!("{name} is not set")))?;
        let bot = bot_owner::telegram_bot_id(&token)
            .ok_or_else(|| refuse(format!("{name} does not hold a Telegram bot token")))?;
        if !bots.insert(bot) {
            return Err(refuse(format!("{platform} bot {bot} is listed twice")));
        }
        if entry.bindings.is_empty() {
            return Err(refuse(
                "no bindings; bind each chat to a chat session".into(),
            ));
        }
        let owner = BotOwner::claim(owners_dir, platform, bot, holder, HANDOFF)
            .map_err(|why| refuse(why.to_string()))?;
        im._owners.push(owner);
        let channel = channel(ImChannelId {
            platform: platform.into(),
            bot,
        });
        let mut bindings = Vec::new();
        for binding in &entry.bindings {
            let target = match BoundProfile::verify(state, &binding.profile).await {
                Ok(profile) => BoundChat::verify(profile, SessionKey(binding.session.clone())),
                Err(error) => Err(error),
            }
            .map_err(|error| refuse(format!("chat {}: {}", binding.chat, error.message)))?;
            bindings.push(ImBinding {
                channel: channel.id(),
                chat: binding.chat,
                sender: binding.sender,
                target,
            });
        }
        let turns = ExternalTurns::new(state.clone(), ledger.clone(), contract_stores());
        let ttl = Duration::from_secs(entry.ttl_secs);
        let allowed = entry.allowed_senders.clone();
        let bridge = ServeImBridge::new(turns, channel, allowed, bindings, ttl).map_err(refuse)?;
        im.bridges.push(bridge);
    }
    Ok(im)
}

/// A bot with no transport: nothing reaches its bridge, and nothing goes out.
struct Unconnected(ImChannelId);

#[async_trait::async_trait]
impl ImChannel for Unconnected {
    fn id(&self) -> ImChannelId {
        self.0.clone()
    }
    async fn send(&self, _chat: i64, _text: &str, _reply_to: Option<i64>) -> eyre::Result<()> {
        eyre::bail!("no transport is connected for this bot")
    }
}

#[cfg(test)]
#[path = "ui_protocol_im_config_tests.rs"]
mod tests;
