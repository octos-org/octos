//! Discord channel using serenity gateway + REST API.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use chrono::Utc;
use eyre::{Result, WrapErr};
use octos_core::{InboundMessage, OutboundMessage};
use reqwest::Client as HttpClient;
use serenity::Client;
use serenity::all::{
    Context, EditMessage, EventHandler, GatewayIntents, Http, Message as DiscordMessage, MessageId,
    ReactionType, Ready,
};
use serenity::builder::{CreateAttachment, CreateEmbed, CreateMessage};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::channel::Channel;
use crate::dedup::MessageDedup;
use crate::media::download_media;

pub struct DiscordChannel {
    token: String,
    http: Arc<Http>,
    allowed_senders: HashSet<String>,
    shutdown: Arc<AtomicBool>,
    media_dir: PathBuf,
    dedup: Arc<MessageDedup>,
    /// #1506 — explicit channel/guild → profile routing (empty = legacy).
    profile_map: crate::profile_map::ChatProfileMap,
}

impl DiscordChannel {
    pub fn new(
        token: &str,
        allowed_senders: Vec<String>,
        shutdown: Arc<AtomicBool>,
        media_dir: PathBuf,
    ) -> Self {
        let http = Arc::new(Http::new(token));
        Self {
            token: token.to_string(),
            http,
            allowed_senders: allowed_senders.into_iter().collect(),
            shutdown,
            media_dir,
            dedup: Arc::new(MessageDedup::new()),
            profile_map: crate::profile_map::ChatProfileMap::default(),
        }
    }

    /// #1506 — install the explicit channel/guild → profile routing map
    /// parsed from this channel entry's settings. A channel-id hit wins over
    /// a guild-id hit; no hit keeps legacy routing.
    pub fn with_profile_map(mut self, profile_map: crate::profile_map::ChatProfileMap) -> Self {
        self.profile_map = profile_map;
        self
    }

    /// Parse an emoji string into a serenity ReactionType.
    /// Supports Unicode emoji (e.g. "👍") and custom emoji format "<:name:id>".
    fn parse_emoji(emoji: &str) -> Result<ReactionType> {
        // Custom emoji format: <:name:id> or <a:name:id>
        if emoji.starts_with('<') && emoji.ends_with('>') {
            let inner = &emoji[1..emoji.len() - 1];
            let parts: Vec<&str> = inner.split(':').collect();
            if parts.len() == 3 {
                let animated = parts[0] == "a";
                let name = parts[1].to_string();
                let id: u64 = parts[2].parse().wrap_err("invalid custom emoji ID")?;
                return Ok(ReactionType::Custom {
                    animated,
                    id: serenity::model::id::EmojiId::new(id),
                    name: Some(name),
                });
            }
        }
        Ok(ReactionType::Unicode(emoji.to_string()))
    }
}

/// Internal handler that forwards Discord messages to the inbound bus.
struct Handler {
    inbound_tx: mpsc::Sender<InboundMessage>,
    allowed_senders: HashSet<String>,
    media_dir: PathBuf,
    download_http: HttpClient,
    dedup: Arc<MessageDedup>,
    /// #1506 — explicit channel/guild → profile routing (empty = legacy).
    profile_map: crate::profile_map::ChatProfileMap,
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, msg: DiscordMessage) {
        if msg.author.bot {
            return;
        }

        // Dedup: skip messages already seen (e.g. on reconnect)
        let msg_id_str = msg.id.to_string();
        if self.dedup.is_duplicate(&msg_id_str) {
            debug!(msg_id = %msg_id_str, "Discord: dedup filtered message");
            return;
        }

        let sender_id = msg.author.id.to_string();

        if !self.allowed_senders.is_empty() && !self.allowed_senders.contains(&sender_id) {
            return;
        }

        // Download attachments
        let mut media = Vec::new();
        for attachment in &msg.attachments {
            let ext = std::path::Path::new(&attachment.filename)
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| format!(".{e}"))
                .unwrap_or_default();
            let filename = format!("{}{}", attachment.id, ext);
            match download_media(
                &self.download_http,
                &attachment.url,
                &[],
                &self.media_dir,
                &filename,
            )
            .await
            {
                Ok(path) => media.push(path.display().to_string()),
                Err(e) => warn!("failed to download Discord attachment: {e}"),
            }
        }

        let guild_id = msg.guild_id.map(|g| g.to_string());
        // #1506 — explicit channel hit wins over the guild fallback; no hit
        // leaves the legacy route untouched. `profile_route_explicit` marks
        // the target as operator-pinned (unresolvable ⇒ refuse, never fall
        // back to the default profile). The whole routing segment lives in
        // `stamp_routing_metadata`; a refusal there drops the message
        // BEFORE any InboundMessage is built or sent.
        let mut metadata = serde_json::json!({
            "message_id": msg.id.to_string(),
            "guild_id": guild_id,
        });
        if self
            .stamp_routing_metadata(
                &SerenityParentLookup(&ctx),
                msg.channel_id,
                guild_id.as_deref(),
                &mut metadata,
            )
            .await
            .is_err()
        {
            return;
        }
        let inbound = InboundMessage {
            channel: "discord".into(),
            sender_id,
            chat_id: msg.channel_id.to_string(),
            content: msg.content.clone(),
            timestamp: Utc::now(),
            media,
            metadata,
            message_id: Some(msg.id.to_string()),
            origin: octos_core::MessageOrigin::ExternalUser,
        };

        if let Err(e) = self.inbound_tx.send(inbound).await {
            error!("Failed to send Discord inbound message: {e}");
        }
    }

    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!(user = %ready.user.name, "Discord bot connected");
    }
}

#[async_trait]
impl Channel for DiscordChannel {
    fn name(&self) -> &str {
        "discord"
    }

    fn max_message_length(&self) -> usize {
        1900
    }

    fn supports_edit(&self) -> bool {
        true
    }

    async fn start(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        info!("Starting Discord channel (gateway)");

        let intents = GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT;

        let handler = Handler {
            inbound_tx,
            allowed_senders: self.allowed_senders.clone(),
            media_dir: self.media_dir.clone(),
            download_http: crate::http_client::channel_http_client(),
            dedup: Arc::clone(&self.dedup),
            profile_map: self.profile_map.clone(),
        };

        let mut client = Client::builder(&self.token, intents)
            .event_handler(handler)
            .await
            .wrap_err("failed to build Discord client")?;

        client.start().await.wrap_err("Discord client error")?;

        info!("Discord channel stopped");
        Ok(())
    }

    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        self.send_with_id(msg).await?;
        Ok(())
    }

    async fn send_with_id(&self, msg: &OutboundMessage) -> Result<Option<String>> {
        let channel_id: u64 = msg
            .chat_id
            .parse()
            .wrap_err_with(|| format!("invalid Discord channel_id: {}", msg.chat_id))?;

        let channel = serenity::model::id::ChannelId::new(channel_id);

        let sent = if !msg.media.is_empty() {
            let mut attachments = Vec::new();
            for path in &msg.media {
                let attachment = CreateAttachment::path(path)
                    .await
                    .wrap_err_with(|| format!("failed to create Discord attachment: {path}"))?;
                attachments.push(attachment);
            }
            let builder = CreateMessage::new().content(&msg.content);
            channel
                .send_files(&*self.http, attachments, builder)
                .await
                .wrap_err("failed to send Discord message with files")?
        } else {
            channel
                .say(&*self.http, &msg.content)
                .await
                .wrap_err("failed to send Discord message")?
        };

        Ok(Some(sent.id.to_string()))
    }

    async fn edit_message(&self, chat_id: &str, message_id: &str, new_content: &str) -> Result<()> {
        let channel_id: u64 = chat_id
            .parse()
            .wrap_err_with(|| format!("invalid Discord channel_id: {chat_id}"))?;
        let msg_id: u64 = message_id
            .parse()
            .wrap_err_with(|| format!("invalid Discord message_id: {message_id}"))?;

        let channel = serenity::model::id::ChannelId::new(channel_id);
        channel
            .edit_message(
                &*self.http,
                MessageId::new(msg_id),
                EditMessage::new().content(new_content),
            )
            .await
            .wrap_err("failed to edit Discord message")?;

        Ok(())
    }

    async fn delete_message(&self, chat_id: &str, message_id: &str) -> Result<()> {
        let channel_id: u64 = chat_id
            .parse()
            .wrap_err_with(|| format!("invalid Discord channel_id: {chat_id}"))?;
        let msg_id: u64 = message_id
            .parse()
            .wrap_err_with(|| format!("invalid Discord message_id: {message_id}"))?;

        let channel = serenity::model::id::ChannelId::new(channel_id);
        channel
            .delete_message(&*self.http, MessageId::new(msg_id))
            .await
            .wrap_err("failed to delete Discord message")?;

        Ok(())
    }

    async fn react_to_message(&self, chat_id: &str, message_id: &str, emoji: &str) -> Result<()> {
        let channel_id: u64 = chat_id
            .parse()
            .wrap_err_with(|| format!("invalid Discord channel_id: {chat_id}"))?;
        let msg_id: u64 = message_id
            .parse()
            .wrap_err_with(|| format!("invalid Discord message_id: {message_id}"))?;

        let reaction = Self::parse_emoji(emoji)?;
        self.http
            .create_reaction(
                serenity::model::id::ChannelId::new(channel_id),
                MessageId::new(msg_id),
                &reaction,
            )
            .await
            .wrap_err("failed to add Discord reaction")?;
        Ok(())
    }

    async fn remove_reaction(&self, chat_id: &str, message_id: &str, emoji: &str) -> Result<()> {
        let channel_id: u64 = chat_id
            .parse()
            .wrap_err_with(|| format!("invalid Discord channel_id: {chat_id}"))?;
        let msg_id: u64 = message_id
            .parse()
            .wrap_err_with(|| format!("invalid Discord message_id: {message_id}"))?;

        let reaction = Self::parse_emoji(emoji)?;
        self.http
            .delete_reaction_me(
                serenity::model::id::ChannelId::new(channel_id),
                MessageId::new(msg_id),
                &reaction,
            )
            .await
            .wrap_err("failed to remove Discord reaction")?;
        Ok(())
    }

    async fn send_embed(
        &self,
        chat_id: &str,
        title: &str,
        description: &str,
        fields: &[(String, String, bool)],
        color: Option<u32>,
    ) -> Result<Option<String>> {
        let channel_id: u64 = chat_id
            .parse()
            .wrap_err_with(|| format!("invalid Discord channel_id: {chat_id}"))?;

        let channel = serenity::model::id::ChannelId::new(channel_id);

        let mut embed = CreateEmbed::new().title(title).description(description);

        if let Some(c) = color {
            embed = embed.color(c);
        }

        for (name, value, inline) in fields {
            embed = embed.field(name, value, *inline);
        }

        let builder = CreateMessage::new().embed(embed);
        let sent = channel
            .send_message(&*self.http, builder)
            .await
            .wrap_err("failed to send Discord embed")?;

        Ok(Some(sent.id.to_string()))
    }

    fn is_allowed(&self, sender_id: &str) -> bool {
        self.allowed_senders.is_empty() || self.allowed_senders.contains(sender_id)
    }

    async fn stop(&self) -> Result<()> {
        self.shutdown.store(true, Ordering::SeqCst);
        Ok(())
    }
}

/// #1506 follow-up (T-0648) — replaceable seam for the thread-parent
/// channel lookup, so the routing boundary (skip / refuse / proceed) is
/// testable without a serenity socket. `Ok(None)` = the channel has no
/// routing parent (a DM, or a non-thread guild channel); `Err` = the lookup
/// itself failed.
#[async_trait]
trait ChannelParentLookup: Send + Sync {
    async fn routing_parent(
        &self,
        channel_id: serenity::model::id::ChannelId,
    ) -> Result<Option<serenity::model::id::ChannelId>, String>;
}

/// The production lookup, backed by the live serenity context.
struct SerenityParentLookup<'a>(&'a Context);

#[async_trait]
impl ChannelParentLookup for SerenityParentLookup<'_> {
    async fn routing_parent(
        &self,
        channel_id: serenity::model::id::ChannelId,
    ) -> Result<Option<serenity::model::id::ChannelId>, String> {
        match channel_id.to_channel(self.0).await {
            Ok(serenity::all::Channel::Guild(channel)) => {
                Ok(routing_parent_id(channel.kind, channel.parent_id))
            }
            // A non-guild (DM) channel has no parent concept; its own id
            // can still be mapped directly.
            Ok(_) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }
}

impl Handler {
    /// #1506 — the routing segment of `message()`: stamp the explicit
    /// route onto `metadata`, or REFUSE the message (`Err`) before any
    /// InboundMessage is built or sent. Control flow:
    ///
    /// - EMPTY map ⇒ pure legacy behavior, the parent lookup is NEVER
    ///   invoked (no per-message HTTP round-trip).
    /// - Guild-only map ⇒ the stamp still runs (the guild fallback applies)
    ///   but the parent lookup is skipped — `route_with_parent` never
    ///   consults a parent without chat entries.
    /// - Chat/channel map ⇒ the lookup is load-bearing: a lookup FAILURE
    ///   refuses the message (never-silently-fall-back, mirroring the
    ///   dispatcher's explicit-route refusal), while `Ok(None)` (DM /
    ///   non-thread channel) proceeds and the channel's own id can still
    ///   be mapped directly.
    async fn stamp_routing_metadata(
        &self,
        lookup: &dyn ChannelParentLookup,
        channel_id: serenity::model::id::ChannelId,
        guild_id: Option<&str>,
        metadata: &mut serde_json::Value,
    ) -> Result<(), ()> {
        if self.profile_map.is_empty() {
            return Ok(());
        }
        let chat_id = channel_id.to_string();
        let parent_channel_id = if self.profile_map.has_chat_entries() {
            match lookup.routing_parent(channel_id).await {
                Ok(parent) => parent.map(|id| id.to_string()),
                Err(error) => {
                    error!(
                        %error,
                        channel_id = %chat_id,
                        "Discord: channel lookup failed with an explicit chat_profile_map \
                         present; dropping the message rather than risking a wrong-profile \
                         delivery (never-silently-fall-back)"
                    );
                    return Err(());
                }
            }
        } else {
            None
        };
        stamp_profile_route(
            &self.profile_map,
            &chat_id,
            parent_channel_id.as_deref(),
            guild_id,
            metadata,
        );
        Ok(())
    }
}

/// #1506 review — a GuildChannel's `parent_id` is the PARENT CHANNEL only
/// for thread kinds; on an ordinary text/voice channel it is the CATEGORY
/// (Serenity docs). Only threads may inherit a parent channel's mapping —
/// an ordinary unmapped text channel must NOT inherit its category's
/// mapping. Pure helper so the distinction is unit-testable without a
/// serenity socket.
fn routing_parent_id(
    kind: serenity::all::ChannelType,
    parent_id: Option<serenity::all::ChannelId>,
) -> Option<serenity::all::ChannelId> {
    use serenity::all::ChannelType::{NewsThread, PrivateThread, PublicThread};
    match kind {
        PublicThread | PrivateThread | NewsThread => parent_id,
        _ => None,
    }
}

/// #1506 — stamp the explicit channel/guild route onto an inbound message.
/// A channel-id hit wins over the guild fallback (see
/// [`crate::profile_map::ChatProfileMap::route`]); no hit leaves the
/// metadata untouched (legacy single-profile routing).
/// `profile_route_explicit` marks the target as operator-pinned:
/// unresolvable ⇒ the dispatcher refuses, never falls back to the default
/// profile.
fn stamp_profile_route(
    map: &crate::profile_map::ChatProfileMap,
    chat_id: &str,
    parent_id: Option<&str>,
    guild_id: Option<&str>,
    metadata: &mut serde_json::Value,
) {
    if let Some(profile_id) = map.route_with_parent(chat_id, parent_id, guild_id) {
        metadata["target_profile_id"] = serde_json::json!(profile_id);
        metadata["profile_route_explicit"] = serde_json::json!(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T-0648 — the behavior evidence lives HERE: these tests drive the
    /// production routing segment (`Handler::stamp_routing_metadata`, the
    /// exact call `message()` makes) through a mock parent lookup that
    /// counts invocations and injects failures. `Handler` is constructible
    /// in tests because every field is either defaulted or a tokio channel.
    struct MockParentLookup {
        calls: std::sync::atomic::AtomicUsize,
        result: std::sync::Mutex<Result<Option<serenity::model::id::ChannelId>, String>>,
    }

    impl MockParentLookup {
        fn ok(parent: Option<serenity::model::id::ChannelId>) -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                result: std::sync::Mutex::new(Ok(parent)),
            }
        }
        fn failing() -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                result: std::sync::Mutex::new(Err("lookup blew up".to_string())),
            }
        }
        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl ChannelParentLookup for MockParentLookup {
        async fn routing_parent(
            &self,
            _channel_id: serenity::model::id::ChannelId,
        ) -> Result<Option<serenity::model::id::ChannelId>, String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.result.lock().unwrap().clone()
        }
    }

    fn handler_with_map(settings: serde_json::Value) -> Handler {
        let (inbound_tx, _rx) = mpsc::channel(1);
        Handler {
            inbound_tx,
            allowed_senders: HashSet::new(),
            media_dir: std::path::PathBuf::from("/tmp/octos-test-media"),
            download_http: HttpClient::new(),
            dedup: Arc::new(MessageDedup::default()),
            profile_map: crate::profile_map::ChatProfileMap::from_settings(&settings).unwrap(),
        }
    }

    /// #1506 follow-up — an EMPTY map never invokes the parent lookup (no
    /// per-message HTTP round-trip) and leaves metadata untouched.
    #[tokio::test]
    async fn should_skip_the_parent_lookup_and_the_stamp_for_an_empty_map() {
        let handler = handler_with_map(serde_json::json!({}));
        let lookup = MockParentLookup::failing();
        let mut metadata = serde_json::json!({"message_id": "m1"});
        handler
            .stamp_routing_metadata(
                &lookup,
                serenity::model::id::ChannelId::new(222),
                None,
                &mut metadata,
            )
            .await
            .expect("an empty map never refuses");
        assert_eq!(lookup.calls(), 0, "no lookup without any map entry");
        assert_eq!(metadata, serde_json::json!({"message_id": "m1"}));
    }

    /// #1506 follow-up — a GUILD-ONLY map still stamps the guild fallback
    /// but never consults the parent lookup, even when it would fail.
    #[tokio::test]
    async fn should_stamp_the_guild_fallback_without_a_parent_lookup_for_a_guild_only_map() {
        let handler = handler_with_map(serde_json::json!({
            "guild_profile_map": {"999": "guild-profile"}
        }));
        let lookup = MockParentLookup::failing();
        let mut metadata = serde_json::json!({"message_id": "m2"});
        handler
            .stamp_routing_metadata(
                &lookup,
                serenity::model::id::ChannelId::new(222),
                Some("999"),
                &mut metadata,
            )
            .await
            .expect("a guild-only map never refuses on a lookup it never makes");
        assert_eq!(lookup.calls(), 0, "no parent lookup without chat entries");
        assert_eq!(metadata["target_profile_id"], "guild-profile");
        assert_eq!(metadata["profile_route_explicit"], true);
    }

    /// #1506 follow-up — with a chat entry present, a lookup FAILURE
    /// refuses the message (fail-closed, never a silent wrong-profile
    /// fallback), and the refusal happens inside the routing segment —
    /// before any InboundMessage could be built or sent by the caller.
    #[tokio::test]
    async fn should_refuse_the_message_when_the_parent_lookup_fails_with_a_chat_map() {
        let handler = handler_with_map(serde_json::json!({
            "chat_profile_map": {"111": "channel-profile"}
        }));
        let lookup = MockParentLookup::failing();
        let mut metadata = serde_json::json!({"message_id": "m3"});
        let outcome = handler
            .stamp_routing_metadata(
                &lookup,
                serenity::model::id::ChannelId::new(222),
                Some("999"),
                &mut metadata,
            )
            .await;
        assert!(outcome.is_err(), "lookup failure must refuse");
        assert_eq!(lookup.calls(), 1);
        // Nothing was stamped — no partial route may leak downstream.
        assert!(metadata.get("target_profile_id").is_none());
        assert!(metadata.get("profile_route_explicit").is_none());
    }

    /// #1506 follow-up — a DM (lookup yields NO parent) must NOT be
    /// refused: its own channel id still maps directly.
    #[tokio::test]
    async fn should_still_map_a_dm_channel_by_its_own_id_when_there_is_no_parent() {
        let handler = handler_with_map(serde_json::json!({
            "chat_profile_map": {"555": "dm-profile"}
        }));
        let lookup = MockParentLookup::ok(None);
        let mut metadata = serde_json::json!({"message_id": "m4"});
        handler
            .stamp_routing_metadata(
                &lookup,
                serenity::model::id::ChannelId::new(555),
                None,
                &mut metadata,
            )
            .await
            .expect("no parent is not a refusal");
        assert_eq!(lookup.calls(), 1);
        assert_eq!(metadata["target_profile_id"], "dm-profile");
    }

    /// #1506 follow-up — a thread under a mapped parent channel follows
    /// the parent's mapping through the real seam (the lookup runs once).
    #[tokio::test]
    async fn should_route_a_thread_via_its_parent_channel_through_the_lookup_seam() {
        let handler = handler_with_map(serde_json::json!({
            "chat_profile_map": {"111": "channel-profile"}
        }));
        let lookup = MockParentLookup::ok(Some(serenity::model::id::ChannelId::new(111)));
        let mut metadata = serde_json::json!({"message_id": "m5"});
        handler
            .stamp_routing_metadata(
                &lookup,
                serenity::model::id::ChannelId::new(222),
                None,
                &mut metadata,
            )
            .await
            .expect("a resolved parent is not a refusal");
        assert_eq!(lookup.calls(), 1);
        assert_eq!(metadata["target_profile_id"], "channel-profile");
    }

    /// #1506 — Discord's stamp point: an explicit hit stamps target +
    /// operator-pinned marker, a thread follows its PARENT channel's
    /// mapping, and an unmapped message keeps legacy metadata. The full
    /// route matrix (chat > parent > guild > none, per-instance maps) lives
    /// in `profile_map::tests`; this pins only the stamp wiring.
    #[test]
    fn should_stamp_explicit_profile_route_on_discord_messages() {
        let map = crate::profile_map::ChatProfileMap::from_settings(&serde_json::json!({
            "chat_profile_map": {"111": "channel-profile"},
            "guild_profile_map": {"999": "guild-profile"}
        }))
        .unwrap();

        let mut metadata = serde_json::json!({"message_id": "m1"});
        stamp_profile_route(&map, "111", None, Some("999"), &mut metadata);
        assert_eq!(metadata["target_profile_id"], "channel-profile");
        assert_eq!(metadata["profile_route_explicit"], true);

        // Thread "222" under the mapped parent channel "111": the parent's
        // mapping applies, not the guild fallback and not the default.
        let mut metadata = serde_json::json!({"message_id": "m2"});
        stamp_profile_route(&map, "222", Some("111"), Some("999"), &mut metadata);
        assert_eq!(metadata["target_profile_id"], "channel-profile");
        assert_eq!(metadata["profile_route_explicit"], true);

        let mut metadata = serde_json::json!({"message_id": "m3"});
        stamp_profile_route(&map, "222", None, Some("888"), &mut metadata);
        assert!(metadata.get("target_profile_id").is_none());
        assert!(metadata.get("profile_route_explicit").is_none());
    }

    /// #1506 review — an ordinary (non-thread) channel's `parent_id` is its
    /// CATEGORY, never a parent channel. The helper returns it only for
    /// thread kinds, and a text channel under a mapped category therefore
    /// does NOT inherit the category's mapping.
    #[test]
    fn should_not_treat_an_ordinary_channels_category_as_a_routable_parent() {
        use serenity::all::{ChannelId, ChannelType};

        let parent = serenity::model::id::ChannelId::new(111);
        for kind in [
            ChannelType::PublicThread,
            ChannelType::PrivateThread,
            ChannelType::NewsThread,
        ] {
            assert_eq!(
                routing_parent_id(kind, Some(parent)),
                Some(parent),
                "{kind:?} inherits its parent channel"
            );
        }
        for kind in [ChannelType::Text, ChannelType::Voice, ChannelType::Forum] {
            assert_eq!(
                routing_parent_id(kind, Some(parent)),
                None,
                "{kind:?}'s parent_id is a category, not a routable parent"
            );
        }

        // End to end through the stamp: a text channel whose category is
        // mapped stays on the default route (no target, no marker), while a
        // thread under that same parent follows the parent's mapping.
        let map = crate::profile_map::ChatProfileMap::from_settings(&serde_json::json!({
            "chat_profile_map": {"111": "category-profile"}
        }))
        .unwrap();
        let mut text_metadata = serde_json::json!({});
        // Ordinary text channel: the helper yields NO parent, so the
        // category id "111" is never consulted as a parent.
        let text_parent = routing_parent_id(
            ChannelType::Text,
            Some(serenity::model::id::ChannelId::new(111)),
        );
        stamp_profile_route(
            &map,
            "222",
            text_parent.map(|id| id.to_string()).as_deref(),
            None,
            &mut text_metadata,
        );
        assert!(text_metadata.get("target_profile_id").is_none());
        assert!(text_metadata.get("profile_route_explicit").is_none());

        let mut thread_metadata = serde_json::json!({});
        let thread_parent = routing_parent_id(
            ChannelType::PublicThread,
            Some(serenity::model::id::ChannelId::new(111)),
        );
        stamp_profile_route(
            &map,
            "222",
            thread_parent.map(|id| id.to_string()).as_deref(),
            None,
            &mut thread_metadata,
        );
        assert_eq!(thread_metadata["target_profile_id"], "category-profile");
    }
}
