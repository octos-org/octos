//! Matrix client (user-account) channel.
//!
//! Unlike [`crate::matrix_channel`] (Appservice mode), this logs in as a regular
//! Matrix user account — via an access token or password — and long-polls the
//! Client-Server `/sync` API to receive messages. No homeserver-side appservice
//! registration is required, so it works with any account on any homeserver
//! (matrix.org, a self-hosted server, etc.).
//!
//! Modeled after the user-mode integration in the sibling `savfox` project, but
//! hand-rolled on the workspace `reqwest` (rustls) client to avoid pulling in a
//! second HTTP stack — consistent with the existing appservice channel.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use eyre::{Result, WrapErr, eyre};
use futures::StreamExt;
use octos_core::{InboundMessage, OutboundMessage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};

use crate::channel::{Channel, ChannelHealth};
use crate::dedup::MessageDedup;
use crate::markdown_html::markdown_to_matrix_html;
use crate::matrix_channel::percent_encode_path;

const CHANNEL_NAME: &str = "matrix";
const EVENT_ROOM_MESSAGE: &str = "m.room.message";
const EVENT_ROOM_MEMBER: &str = "m.room.member";
const EVENT_ROOM_NAME: &str = "m.room.name";
const EVENT_ROOM_CANONICAL_ALIAS: &str = "m.room.canonical_alias";
const EVENT_M_DIRECT: &str = "m.direct";
const MSGTYPE_TEXT: &str = "m.text";
const MATRIX_INVITES_FILE: &str = "matrix-invites.json";
/// Long-poll timeout for the `/sync` request (server holds the connection open
/// until an event arrives or this elapses).
const SYNC_TIMEOUT_MS: u64 = 30_000;
const MATRIX_ERROR_BODY_MAX_BYTES: usize = 2048;
const DEFAULT_DEVICE_NAME: &str = "octos";
/// How long a `joined_members` probe result is trusted before re-checking
/// whether a room is a 1:1 (mirrors the appservice channel's TTL).
const DM_PROBE_CACHE_TTL: Duration = Duration::from_secs(60);
/// How long a FAILED probe suppresses further probes; the failure itself
/// still fails open (answer) on every candidate message.
const DM_PROBE_FAILURE_CACHE_TTL: Duration = Duration::from_secs(10);
/// Bound on a single `joined_members` probe request. Unlike `/sync` the probe
/// has no server-side backstop, so without this a hung homeserver would stall
/// the forward loop on the first suppression candidate.
const DM_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Matrix invite auto-join policy.
///
/// Mirrors OpenClaw's Matrix model: invites are evaluated before the runtime
/// can reliably classify a room as a DM or a group, so this policy applies to
/// every invite.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MatrixAutoJoin {
    #[default]
    Off,
    Allowlist,
    Always,
}

impl MatrixAutoJoin {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "always" | "on" | "true" => Self::Always,
            "allowlist" | "allow_list" | "allowed" => Self::Allowlist,
            _ => Self::Off,
        }
    }
}

/// Matrix room/group authorization policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MatrixGroupPolicy {
    Open,
    #[default]
    Allowlist,
    Disabled,
}

impl MatrixGroupPolicy {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "open" | "all" => Self::Open,
            "disabled" | "off" | "false" => Self::Disabled,
            _ => Self::Allowlist,
        }
    }
}

/// How the channel treats group-room messages that explicitly mention other
/// users but not this bot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MatrixMentionPolicy {
    /// Stay silent: a message with explicit mentions targeting someone else
    /// is directed at them, not at the room — answering it would make every
    /// bot in a multi-bot room reply to every addressed message.
    #[default]
    Strict,
    /// Answer regardless of who is mentioned (pre-#1547 behaviour).
    Open,
}

impl MatrixMentionPolicy {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "open" => Self::Open,
            _ => Self::Strict,
        }
    }
}

/// Credentials resolved after a successful login (token reuse or password).
#[derive(Clone, Debug)]
struct ResolvedClient {
    access_token: String,
    user_id: String,
    logout_on_stop: bool,
}

/// A single text message extracted from a `/sync` response.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ParsedMessage {
    room_id: String,
    sender: String,
    body: String,
    event_id: Option<String>,
    mentioned_self: bool,
    /// Users other than this bot explicitly mentioned by the message
    /// (`m.mentions`, a matrix.to pill, or a hand-typed MXID). Empty when
    /// only the bot (or nobody) is addressed.
    other_mentions: Vec<String>,
}

/// A room invite extracted from `/sync`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ParsedInvite {
    room_id: String,
    room_name: Option<String>,
    canonical_alias: Option<String>,
    inviter: Option<String>,
    membership_event_id: Option<String>,
}

impl ParsedInvite {
    fn pending(&self, channel_index: usize) -> MatrixPendingInvite {
        let now = Utc::now();
        MatrixPendingInvite {
            channel_index,
            room_id: self.room_id.clone(),
            room_name: self.room_name.clone(),
            canonical_alias: self.canonical_alias.clone(),
            inviter: self.inviter.clone(),
            membership_event_id: self.membership_event_id.clone(),
            received_at: now,
            last_seen_at: now,
            dismissed_at: None,
        }
    }
}

/// An invite waiting for an administrator decision.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MatrixPendingInvite {
    /// Index in the profile's channel list. This keeps the on-disk schema ready
    /// for multiple Matrix account channels without changing the API later.
    #[serde(default)]
    pub channel_index: usize,
    pub room_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canonical_alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inviter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub membership_event_id: Option<String>,
    pub received_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dismissed_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct MatrixInviteStoreData {
    #[serde(default)]
    invites: Vec<MatrixPendingInvite>,
}

/// Small JSON-backed store for Matrix invites that require admin review.
#[derive(Clone, Debug)]
pub struct MatrixInviteStore {
    path: PathBuf,
}

impl MatrixInviteStore {
    pub fn for_profile_data_dir(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join(MATRIX_INVITES_FILE),
        }
    }

    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load_data(&self) -> Result<MatrixInviteStoreData> {
        match fs::read_to_string(&self.path) {
            Ok(body) => serde_json::from_str(&body).wrap_err_with(|| {
                format!(
                    "failed to parse Matrix invite store: {}",
                    self.path.display()
                )
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(MatrixInviteStoreData::default()),
            Err(e) => Err(e).wrap_err_with(|| {
                format!(
                    "failed to read Matrix invite store: {}",
                    self.path.display()
                )
            }),
        }
    }

    fn save_data(&self, data: &MatrixInviteStoreData) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).wrap_err_with(|| {
                format!("failed to create Matrix invite dir: {}", parent.display())
            })?;
        }
        let body =
            serde_json::to_string_pretty(data).wrap_err("failed to serialize Matrix invites")?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, body)
            .wrap_err_with(|| format!("failed to write Matrix invite store: {}", tmp.display()))?;
        fs::rename(&tmp, &self.path).wrap_err_with(|| {
            format!(
                "failed to replace Matrix invite store: {}",
                self.path.display()
            )
        })?;
        Ok(())
    }

    pub fn list(&self, include_dismissed: bool) -> Result<Vec<MatrixPendingInvite>> {
        let mut invites = self.load_data()?.invites;
        if !include_dismissed {
            invites.retain(|invite| invite.dismissed_at.is_none());
        }
        invites.sort_by_key(|b| std::cmp::Reverse(b.last_seen_at));
        Ok(invites)
    }

    pub fn upsert(&self, invite: MatrixPendingInvite) -> Result<()> {
        let mut data = self.load_data()?;
        match data.invites.iter_mut().find(|existing| {
            existing.channel_index == invite.channel_index && existing.room_id == invite.room_id
        }) {
            Some(existing) => {
                let membership_changed = invite.membership_event_id.is_some()
                    && existing.membership_event_id != invite.membership_event_id;
                existing.room_name = invite.room_name;
                existing.canonical_alias = invite.canonical_alias;
                existing.inviter = invite.inviter;
                existing.membership_event_id = invite.membership_event_id;
                existing.last_seen_at = invite.last_seen_at;
                if membership_changed {
                    existing.received_at = invite.received_at;
                    existing.dismissed_at = None;
                }
            }
            None => data.invites.push(invite),
        }
        self.save_data(&data)
    }

    pub fn remove(&self, channel_index: usize, room_id: &str) -> Result<bool> {
        let mut data = self.load_data()?;
        let original_len = data.invites.len();
        data.invites
            .retain(|invite| invite.channel_index != channel_index || invite.room_id != room_id);
        let removed = data.invites.len() != original_len;
        if removed {
            self.save_data(&data)?;
        }
        Ok(removed)
    }

    pub fn dismiss(&self, channel_index: usize, room_id: &str) -> Result<bool> {
        let mut data = self.load_data()?;
        let mut changed = false;
        let now = Utc::now();
        for invite in &mut data.invites {
            if invite.channel_index == channel_index && invite.room_id == room_id {
                invite.dismissed_at = Some(now);
                changed = true;
            }
        }
        if changed {
            self.save_data(&data)?;
        }
        Ok(changed)
    }
}

/// Structured result of parsing one `/sync` response body.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ParsedSync {
    next_batch: Option<String>,
    /// Room invites the account received (candidates for auto-join or review).
    invites: Vec<ParsedInvite>,
    messages: Vec<ParsedMessage>,
    /// Replacement set of DM room IDs when the sync carried an `m.direct`
    /// account-data event; `None` when it did not (keep the previous set).
    direct_rooms: Option<Vec<String>>,
}

/// Parse a Matrix `/sync` response into auto-join invites and inbound text
/// messages, skipping the account's own messages.
fn parse_sync(payload: &Value, self_user_id: &str) -> ParsedSync {
    let next_batch = payload
        .get("next_batch")
        .and_then(Value::as_str)
        .map(str::to_owned);

    let rooms = payload.get("rooms");

    let invites = rooms
        .and_then(|r| r.get("invite"))
        .and_then(Value::as_object)
        .map(|obj| {
            obj.iter()
                .map(|(room_id, room)| parse_invite(room_id, room, self_user_id))
                .collect()
        })
        .unwrap_or_default();

    let mut messages = Vec::new();
    if let Some(joined) = rooms.and_then(|r| r.get("join")).and_then(Value::as_object) {
        for (room_id, room) in joined {
            let events = room
                .get("timeline")
                .and_then(|t| t.get("events"))
                .and_then(Value::as_array);
            let Some(events) = events else { continue };

            for event in events {
                if event.get("type").and_then(Value::as_str) != Some(EVENT_ROOM_MESSAGE) {
                    continue;
                }
                let sender = event.get("sender").and_then(Value::as_str).unwrap_or("");
                if sender.is_empty() || sender.eq_ignore_ascii_case(self_user_id) {
                    continue;
                }
                let content = match event.get("content") {
                    Some(c) => c,
                    None => continue,
                };
                if content.get("msgtype").and_then(Value::as_str) != Some(MSGTYPE_TEXT) {
                    continue;
                }
                let body = content.get("body").and_then(Value::as_str).unwrap_or("");
                if body.is_empty() {
                    continue;
                }
                let event_id = event
                    .get("event_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let mentioned_self = content_mentions_user(content, self_user_id)
                    || contains_matrix_user_id_mention(body, self_user_id);
                let other_mentions = content_other_mentions(content, body, self_user_id);
                messages.push(ParsedMessage {
                    room_id: room_id.clone(),
                    sender: sender.to_owned(),
                    body: body.to_owned(),
                    event_id,
                    mentioned_self,
                    other_mentions,
                });
            }
        }
    }

    ParsedSync {
        next_batch,
        invites,
        messages,
        direct_rooms: parse_direct_rooms(payload),
    }
}

/// Extract the DM room set from an `m.direct` account-data event, if the sync
/// payload carries one. The event content maps user IDs to their direct-chat
/// room lists; the union of every list is the account's DM set.
fn parse_direct_rooms(payload: &Value) -> Option<Vec<String>> {
    let content = payload
        .get("account_data")
        .and_then(|a| a.get("events"))
        .and_then(Value::as_array)?
        .iter()
        .find(|event| event.get("type").and_then(Value::as_str) == Some(EVENT_M_DIRECT))?
        .get("content")?;
    let mut rooms = Vec::new();
    for room_list in content.as_object()?.values() {
        if let Some(ids) = room_list.as_array() {
            rooms.extend(ids.iter().filter_map(Value::as_str).map(str::to_owned));
        }
    }
    Some(rooms)
}

fn parse_invite(room_id: &str, room: &Value, self_user_id: &str) -> ParsedInvite {
    let mut parsed = ParsedInvite {
        room_id: room_id.to_owned(),
        room_name: None,
        canonical_alias: None,
        inviter: None,
        membership_event_id: None,
    };

    let Some(events) = room
        .get("invite_state")
        .and_then(|state| state.get("events"))
        .and_then(Value::as_array)
    else {
        return parsed;
    };

    for event in events {
        let event_type = event.get("type").and_then(Value::as_str);
        let null_content = Value::Null;
        let content = event.get("content").unwrap_or(&null_content);
        match event_type {
            Some(EVENT_ROOM_NAME) if parsed.room_name.is_none() => {
                parsed.room_name = content
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToOwned::to_owned);
            }
            Some(EVENT_ROOM_CANONICAL_ALIAS) if parsed.canonical_alias.is_none() => {
                parsed.canonical_alias = content
                    .get("alias")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToOwned::to_owned);
            }
            Some(EVENT_ROOM_MEMBER) => {
                let membership = content.get("membership").and_then(Value::as_str);
                let state_key = event.get("state_key").and_then(Value::as_str);
                if membership == Some("invite")
                    && (state_key.is_none() || state_key == Some(self_user_id))
                {
                    parsed.inviter = event
                        .get("sender")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    parsed.membership_event_id = event
                        .get("event_id")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
            }
            _ => {}
        }
    }

    parsed
}

/// Build the `/sync` request path with optional `since` token.
fn sync_path(since: Option<&str>, timeout_ms: u64) -> String {
    let mut path = format!("/_matrix/client/v3/sync?timeout={timeout_ms}");
    if let Some(since) = since.map(str::trim).filter(|s| !s.is_empty()) {
        path.push_str("&since=");
        path.push_str(&percent_encode_path(since));
    }
    path
}

fn content_mentions_user(content: &Value, user_id: &str) -> bool {
    content
        .get("m.mentions")
        .and_then(|m| m.get("user_ids"))
        .and_then(Value::as_array)
        .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(user_id)))
}

/// The users other than `self_user_id` explicitly mentioned by the message,
/// via a structured `m.mentions` entry, a matrix.to pill in `formatted_body`,
/// or a hand-typed MXID in the plain body.
///
/// Rich-reply context is not explicit addressing: clients auto-include the
/// replied-to sender in `m.mentions` and embed an `<mx-reply>` fallback with
/// a matrix.to pill plus `> `-quoted lines. All three are stripped first, so
/// a reply that addresses the bot by name is still answered.
fn content_other_mentions(content: &Value, body: &str, self_user_id: &str) -> Vec<String> {
    let replied_to = reply_fallback_sender(content, body);
    let mut mentions: Vec<String> = Vec::new();
    let mut push = |id: &str| {
        if id.eq_ignore_ascii_case(self_user_id)
            || replied_to.is_some_and(|rt| id.eq_ignore_ascii_case(rt))
            || mentions.iter().any(|m| m.eq_ignore_ascii_case(id))
        {
            return;
        }
        mentions.push(id.to_owned());
    };

    if let Some(ids) = content
        .get("m.mentions")
        .and_then(|m| m.get("user_ids"))
        .and_then(Value::as_array)
    {
        for id in ids.iter().filter_map(Value::as_str) {
            push(id);
        }
    }

    if let Some(formatted_body) = content.get("formatted_body").and_then(Value::as_str) {
        let mut rest = strip_mx_reply(formatted_body);
        while let Some(at) = rest.find("matrix.to/#/@") {
            rest = &rest[at + "matrix.to/#/".len()..];
            if let Some(mxid) = scan_mxid(rest, 0) {
                push(mxid);
            }
        }
    }

    for id in text_other_mentions(
        strip_reply_fallback(content, body),
        self_user_id,
        replied_to,
    ) {
        push(&id);
    }
    mentions
}

/// The MXID a rich reply falls back to, if this event is a reply
/// (`m.relates_to.m.in_reply_to`) and the plain body starts with the spec
/// fallback quote `> <@sender:server> …`.
fn reply_fallback_sender<'a>(content: &Value, body: &'a str) -> Option<&'a str> {
    content.get("m.relates_to")?.get("m.in_reply_to")?;
    let first_line = body.lines().next()?.strip_prefix("> ")?.trim_start();
    let inner = first_line.strip_prefix('<')?;
    let end = inner.find('>')?;
    let mxid = &inner[..end];
    mxid.starts_with('@').then_some(mxid)
}

/// Remove the leading `<mx-reply>…</mx-reply>` fallback block from a formatted
/// body; its pills quote the original message, they are not new mentions.
fn strip_mx_reply(formatted_body: &str) -> &str {
    let trimmed = formatted_body.trim_start();
    if !trimmed.starts_with("<mx-reply>") {
        return formatted_body;
    }
    match trimmed.find("</mx-reply>") {
        Some(end) => &trimmed[end + "</mx-reply>".len()..],
        None => formatted_body,
    }
}

/// Drop a rich reply's leading `> `-quoted fallback lines (and the blank
/// separator) so quoted MXIDs are not read as new mentions. Non-reply events
/// pass through unchanged.
fn strip_reply_fallback<'a>(content: &Value, body: &'a str) -> &'a str {
    let is_reply = content
        .get("m.relates_to")
        .and_then(|r| r.get("m.in_reply_to"))
        .is_some();
    if !is_reply {
        return body;
    }
    let mut rest = body;
    for line in body.lines() {
        if !(line == ">" || line.starts_with("> ") || line.trim().is_empty()) {
            break;
        }
        rest = &rest[line.len()..];
        rest = rest
            .strip_prefix("\r\n")
            .or_else(|| rest.strip_prefix('\n'))
            .unwrap_or(rest);
    }
    rest
}

/// Scan `text` for a hand-typed MXID (`@localpart:server`) starting at byte
/// offset `start`. Returns the matched token. The server part must contain a
/// dot or be `localhost` so plain English like "@alice: hi" does not count.
fn scan_mxid(text: &str, start: usize) -> Option<&str> {
    let bytes = text.as_bytes();
    if bytes.get(start) != Some(&b'@') {
        return None;
    }
    let local_start = start + 1;
    let mut i = local_start;
    while i < bytes.len() && is_mxid_localpart_byte(bytes[i]) {
        i += 1;
    }
    if i == local_start || bytes.get(i) != Some(&b':') {
        return None;
    }
    let server_start = i + 1;
    i = server_start;
    while i < bytes.len() && is_mxid_server_byte(bytes[i]) {
        i += 1;
    }
    // An optional `:port` suffix is part of the server name.
    if bytes.get(i) == Some(&b':') && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    // A trailing dot is sentence punctuation, not part of the server name.
    while i > server_start && bytes[i - 1] == b'.' {
        i -= 1;
    }
    let server = &text[server_start..i];
    let host = server.split(':').next().unwrap_or("");
    if host != "localhost" && !host.contains('.') {
        return None;
    }
    Some(&text[start..i])
}

fn is_mxid_localpart_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/' | b'=' | b'+')
}

fn is_mxid_server_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-')
}

/// The hand-typed MXID mentions in `text` targeting users other than
/// `self_user_id` (and, inside a reply, other than the replied-to sender).
/// The `@` must start a token (preceded by whitespace or opening
/// punctuation) so email addresses do not count.
fn text_other_mentions(text: &str, self_user_id: &str, replied_to: Option<&str>) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut mentions: Vec<String> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'@' {
            i += 1;
            continue;
        }
        let before_ok = text[..i]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || matches!(c, '<' | '(' | '['));
        if before_ok && let Some(mxid) = scan_mxid(text, i) {
            let is_replied_to = replied_to.is_some_and(|rt| mxid.eq_ignore_ascii_case(rt));
            if !mxid.eq_ignore_ascii_case(self_user_id)
                && !is_replied_to
                && !mentions.iter().any(|m| m.eq_ignore_ascii_case(mxid))
            {
                mentions.push(mxid.to_owned());
            }
            i += mxid.len();
            continue;
        }
        i += 1;
    }
    mentions
}

fn contains_matrix_user_id_mention(text: &str, user_id: &str) -> bool {
    let Some(start) = text.find(user_id) else {
        return false;
    };
    let before_ok = text[..start]
        .chars()
        .next_back()
        .is_none_or(|c| c.is_whitespace() || matches!(c, '<' | '(' | '['));
    let end = start + user_id.len();
    let after_ok = text[end..].chars().next().is_none_or(|c| {
        c.is_whitespace() || matches!(c, ':' | ',' | '.' | '!' | '?' | ')' | ']' | '>')
    });
    before_ok && after_ok
}

fn is_slash_command(text: &str) -> bool {
    text.trim_start().starts_with('/')
}

fn is_stable_join_target(target: &str) -> bool {
    let trimmed = target.trim();
    trimmed == "*" || trimmed.starts_with('!') || trimmed.starts_with('#')
}

fn matrix_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build Matrix HTTP client")
}

async fn matrix_error_body(resp: reqwest::Response) -> String {
    let mut buf = Vec::new();
    let mut truncated = resp
        .content_length()
        .map(|len| len > MATRIX_ERROR_BODY_MAX_BYTES as u64)
        .unwrap_or(false);
    let mut stream = resp.bytes_stream();

    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => {
                if buf.len() + chunk.len() > MATRIX_ERROR_BODY_MAX_BYTES {
                    let remaining = MATRIX_ERROR_BODY_MAX_BYTES.saturating_sub(buf.len());
                    buf.extend_from_slice(&chunk[..remaining]);
                    truncated = true;
                    break;
                }
                buf.extend_from_slice(&chunk);
            }
            Err(_) => {
                truncated = true;
                break;
            }
        }
    }

    sanitize_matrix_error_body(&String::from_utf8_lossy(&buf), truncated)
}

fn sanitize_matrix_error_body(raw: &str, truncated: bool) -> String {
    let mut out = String::new();
    let mut last_space = false;
    for ch in raw.chars() {
        let ch = if ch.is_control() { ' ' } else { ch };
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(ch);
            last_space = false;
        }
    }

    let mut out = out.trim().to_string();
    if truncated {
        if out.is_empty() {
            out.push_str("[truncated]");
        } else {
            out.push_str(" ... [truncated]");
        }
    }
    out
}

#[cfg(test)]
fn initial_sync_retry_delay() -> Duration {
    Duration::from_millis(10)
}

#[cfg(not(test))]
fn initial_sync_retry_delay() -> Duration {
    Duration::from_secs(1)
}

/// Build the JSON body for a `m.login.password` request.
fn password_login_body(user_id: &str, password: &str, device_name: Option<&str>) -> Value {
    json!({
        "type": "m.login.password",
        "identifier": { "type": "m.id.user", "user": user_id },
        "password": password,
        "initial_device_display_name": device_name.unwrap_or(DEFAULT_DEVICE_NAME),
    })
}

/// Matrix user-account channel.
///
/// Authenticates with a regular Matrix account and receives messages by
/// long-polling the Client-Server `/sync` API. Outbound messages are sent via
/// `PUT .../send/m.room.message/{txn_id}` as that account.
pub struct MatrixUserChannel {
    homeserver: String,
    user_id: Option<String>,
    access_token: Option<String>,
    password: Option<String>,
    device_name: Option<String>,
    /// Room allowlist used when `group_policy` is `Allowlist`.
    rooms: Vec<String>,
    auto_join: MatrixAutoJoin,
    auto_join_allowlist: Vec<String>,
    group_policy: MatrixGroupPolicy,
    require_mention: bool,
    mention_policy: MatrixMentionPolicy,
    channel_index: usize,
    invite_store: Option<MatrixInviteStore>,
    /// Optional sender allowlist. Empty means any sender in an allowed room.
    allowed_senders: HashSet<String>,
    /// Rooms the account has marked as direct chats (`m.direct` account
    /// data). Mention suppression does not apply inside DMs.
    direct_rooms: Mutex<HashSet<String>>,
    /// `joined_members` probe results per room (`room_id -> (outcome,
    /// fetched_at)`), so the mention gate does not hit the homeserver on
    /// every suppression candidate. `Some(is_dm)` entries expire after
    /// [`DM_PROBE_CACHE_TTL`]; failed probes (`None`) are cached only for
    /// [`DM_PROBE_FAILURE_CACHE_TTL`] and keep failing open.
    dm_probe_cache: Mutex<HashMap<String, (Option<bool>, Instant)>>,
    /// Rooms whose first mention-gate suppression was already logged at info.
    /// The gate flips pre-#1547 behaviour for `require_mention: false`
    /// deployments, so the first suppression per room is announced at info
    /// (with the policy and the mention set) for operators to notice;
    /// repeats stay at debug to avoid log spam on a busy room.
    suppression_logged_rooms: Mutex<HashSet<String>>,
    shutdown: Arc<AtomicBool>,
    http: reqwest::Client,
    dedup: Arc<MessageDedup>,
    /// Populated on `start()` after a successful login; reused by `send()`.
    resolved: Mutex<Option<ResolvedClient>>,
}

impl MatrixUserChannel {
    /// Create a new user-mode Matrix channel.
    ///
    /// Either `access_token` or (`user_id` + `password`) must be provided; this
    /// is validated lazily at `start()` so construction never fails.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        homeserver: &str,
        user_id: Option<String>,
        access_token: Option<String>,
        password: Option<String>,
        device_name: Option<String>,
        rooms: Vec<String>,
        auto_join: MatrixAutoJoin,
        auto_join_allowlist: Vec<String>,
        group_policy: MatrixGroupPolicy,
        require_mention: bool,
        mention_policy: MatrixMentionPolicy,
        allowed_senders: Vec<String>,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        Self {
            homeserver: homeserver.trim_end_matches('/').to_string(),
            user_id: user_id.filter(|s| !s.trim().is_empty()),
            access_token: access_token.filter(|s| !s.trim().is_empty()),
            password: password.filter(|s| !s.trim().is_empty()),
            device_name: device_name.filter(|s| !s.trim().is_empty()),
            rooms: rooms
                .into_iter()
                .map(|r| r.trim().to_owned())
                .filter(|r| !r.is_empty())
                .collect(),
            auto_join,
            auto_join_allowlist: auto_join_allowlist
                .into_iter()
                .map(|r| r.trim().to_owned())
                .filter(|r| !r.is_empty())
                .collect(),
            group_policy,
            require_mention,
            mention_policy,
            channel_index: 0,
            invite_store: None,
            allowed_senders: allowed_senders
                .into_iter()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect(),
            direct_rooms: Mutex::new(HashSet::new()),
            dm_probe_cache: Mutex::new(HashMap::new()),
            suppression_logged_rooms: Mutex::new(HashSet::new()),
            shutdown,
            http: matrix_http_client(),
            dedup: Arc::new(MessageDedup::new()),
            resolved: Mutex::new(None),
        }
    }

    pub fn with_channel_index(mut self, channel_index: usize) -> Self {
        self.channel_index = channel_index;
        self
    }

    pub fn with_invite_store(mut self, invite_store: MatrixInviteStore) -> Self {
        self.invite_store = Some(invite_store);
        self
    }

    fn api_url(&self, path: &str) -> String {
        format!("{}{}", self.homeserver, path)
    }

    /// Resolve credentials: validate an access token via `whoami`, or perform a
    /// password login. Returns the access token and canonical user ID.
    async fn resolve_login(&self) -> Result<ResolvedClient> {
        if let Some(token) = self.access_token.as_deref() {
            let url = self.api_url("/_matrix/client/v3/account/whoami");
            let resp = self
                .http
                .get(&url)
                .bearer_auth(token)
                .send()
                .await
                .wrap_err("Matrix whoami request failed")?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = matrix_error_body(resp).await;
                return Err(eyre!("Matrix whoami failed (status={status}): {body}"));
            }
            let payload: Value = resp.json().await.wrap_err("invalid whoami response")?;
            let user_id = payload
                .get("user_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| self.user_id.clone())
                .ok_or_else(|| eyre!("whoami response missing user_id"))?;
            return Ok(ResolvedClient {
                access_token: token.to_owned(),
                user_id,
                logout_on_stop: false,
            });
        }

        let user_id = self.user_id.as_deref().ok_or_else(|| {
            eyre!("matrix user channel requires access_token or user_id+password")
        })?;
        let password = self
            .password
            .as_deref()
            .ok_or_else(|| eyre!("matrix user channel requires a password when no access_token"))?;

        let url = self.api_url("/_matrix/client/v3/login");
        let body = password_login_body(user_id, password, self.device_name.as_deref());
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .wrap_err("Matrix password login request failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = matrix_error_body(resp).await;
            return Err(eyre!(
                "Matrix password login failed (status={status}): {body}"
            ));
        }
        let payload: Value = resp.json().await.wrap_err("invalid login response")?;
        let access_token = payload
            .get("access_token")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| eyre!("login response missing access_token"))?;
        let resolved_user_id = payload
            .get("user_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| user_id.to_owned());
        Ok(ResolvedClient {
            access_token,
            user_id: resolved_user_id,
            logout_on_stop: true,
        })
    }

    /// Perform a single `/sync` request and return the parsed response.
    async fn sync_once(
        &self,
        token: &str,
        self_user_id: &str,
        since: Option<&str>,
        timeout_ms: u64,
    ) -> Result<ParsedSync> {
        let url = self.api_url(&sync_path(since, timeout_ms));
        let resp = self
            .http
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .wrap_err("Matrix sync request failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = matrix_error_body(resp).await;
            return Err(eyre!("Matrix sync failed (status={status}): {body}"));
        }
        let payload: Value = resp.json().await.wrap_err("invalid sync response")?;
        Ok(parse_sync(&payload, self_user_id))
    }

    /// Join a room by ID (used to auto-accept invites).
    async fn join_room(&self, token: &str, room_id: &str) -> Result<()> {
        let url = self.api_url(&format!(
            "/_matrix/client/v3/rooms/{}/join",
            percent_encode_path(room_id)
        ));
        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&json!({}))
            .send()
            .await
            .wrap_err("Matrix join room request failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = matrix_error_body(resp).await;
            return Err(eyre!("Matrix join room failed (status={status}): {body}"));
        }
        Ok(())
    }

    async fn resolve_room_alias(&self, token: &str, alias: &str) -> Result<Option<String>> {
        let url = self.api_url(&format!(
            "/_matrix/client/v3/directory/room/{}",
            percent_encode_path(alias)
        ));
        let resp = self
            .http
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .wrap_err("Matrix room alias lookup failed")?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = matrix_error_body(resp).await;
            return Err(eyre!(
                "Matrix room alias lookup failed (status={status}): {body}"
            ));
        }
        let payload: Value = resp
            .json()
            .await
            .wrap_err("invalid alias lookup response")?;
        Ok(payload
            .get("room_id")
            .and_then(Value::as_str)
            .map(str::to_owned))
    }

    async fn auto_join_allowed(&self, token: &str, room_id: &str) -> bool {
        match self.auto_join {
            MatrixAutoJoin::Off => false,
            MatrixAutoJoin::Always => true,
            MatrixAutoJoin::Allowlist => {
                let targets = if self.auto_join_allowlist.is_empty() {
                    // Backward compatibility for early user-mode configs where
                    // `rooms` doubled as the invite allowlist.
                    &self.rooms
                } else {
                    &self.auto_join_allowlist
                };
                for target in targets {
                    let trimmed = target.trim();
                    if !is_stable_join_target(trimmed) {
                        warn!(
                            target = trimmed,
                            "Matrix auto-join target ignored; use !room_id, #alias, or *"
                        );
                        continue;
                    }
                    if trimmed == "*" || trimmed == room_id {
                        return true;
                    }
                    if trimmed.starts_with('#') {
                        match self.resolve_room_alias(token, trimmed).await {
                            Ok(Some(resolved)) if resolved == room_id => return true,
                            Ok(_) => {}
                            Err(e) => warn!(
                                target = trimmed,
                                error = %e,
                                "Matrix auto-join alias resolution failed"
                            ),
                        }
                    }
                }
                false
            }
        }
    }

    /// Whether a room passes the configured room/group policy.
    fn room_allowed(&self, room_id: &str) -> bool {
        match self.group_policy {
            MatrixGroupPolicy::Disabled => false,
            MatrixGroupPolicy::Open => true,
            MatrixGroupPolicy::Allowlist => self.rooms.iter().any(|r| r == "*" || r == room_id),
        }
    }

    fn sender_allowed(&self, sender_id: &str) -> bool {
        self.allowed_senders.is_empty() || self.allowed_senders.contains(sender_id)
    }

    fn record_pending_invite(&self, invite: &ParsedInvite) {
        let Some(store) = &self.invite_store else {
            return;
        };
        if let Err(e) = store.upsert(invite.pending(self.channel_index)) {
            warn!(
                room_id = %invite.room_id,
                error = %e,
                "failed to record pending Matrix invite"
            );
        }
    }

    fn clear_pending_invite(&self, room_id: &str) {
        let Some(store) = &self.invite_store else {
            return;
        };
        if let Err(e) = store.remove(self.channel_index, room_id) {
            warn!(room_id, error = %e, "failed to clear pending Matrix invite");
        }
    }

    async fn join_allowed_invites(&self, token: &str, invites: Vec<ParsedInvite>) {
        for invite in invites {
            if !self.auto_join_allowed(token, &invite.room_id).await {
                debug!(
                    room_id = %invite.room_id,
                    auto_join = ?self.auto_join,
                    "Matrix invite queued for admin review by auto-join policy"
                );
                self.record_pending_invite(&invite);
                continue;
            }
            if let Err(e) = self.join_room(token, &invite.room_id).await {
                warn!(room_id = %invite.room_id, error = %e, "Matrix auto-join failed");
                self.record_pending_invite(&invite);
            } else {
                self.clear_pending_invite(&invite.room_id);
            }
        }
    }

    async fn initial_sync_cursor(&self, token: &str, self_user_id: &str) -> Result<Option<String>> {
        let mut backoff = initial_sync_retry_delay();
        while !self.shutdown.load(Ordering::Acquire) {
            match self.sync_once(token, self_user_id, None, 0).await {
                Ok(initial) => {
                    self.update_direct_rooms(initial.direct_rooms).await;
                    self.join_allowed_invites(token, initial.invites).await;
                    if let Some(next_batch) = initial.next_batch {
                        return Ok(Some(next_batch));
                    }
                    warn!("initial Matrix sync response missing next_batch; retrying");
                }
                Err(e) => {
                    warn!(error = %e, "initial Matrix sync failed; retrying");
                }
            }

            if self.shutdown.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
        Ok(None)
    }

    /// Replace the known DM set when a sync carried `m.direct` account data.
    async fn update_direct_rooms(&self, direct_rooms: Option<Vec<String>>) {
        if let Some(rooms) = direct_rooms {
            *self.direct_rooms.lock().await = rooms.into_iter().collect();
        }
    }

    /// Whether `room_id` should be treated as a 1:1 direct chat: rooms listed
    /// in the account's `m.direct`, or rooms whose joined membership is just
    /// this account plus at most one other user (probed via the homeserver,
    /// cached briefly). `m.direct` is only ever populated if this account's
    /// own client set it, so in practice the probe is what makes the DM
    /// exemption work. When membership cannot be determined, fail toward the
    /// pre-#1547 behaviour (answer) rather than extending the new gate's
    /// reach on a homeserver error.
    async fn is_direct_room(&self, room_id: &str) -> bool {
        if self.direct_rooms.lock().await.contains(room_id) {
            return true;
        }
        let cached = self.dm_probe_cache.lock().await.get(room_id).copied();
        if let Some((outcome, fetched_at)) = cached {
            let ttl = match outcome {
                Some(_) => DM_PROBE_CACHE_TTL,
                None => DM_PROBE_FAILURE_CACHE_TTL,
            };
            if fetched_at.elapsed() < ttl {
                // A cached failure fails open, like a fresh one.
                return outcome.unwrap_or(true);
            }
        }
        let outcome = self.probe_room_is_dm(room_id).await;
        let mut cache = self.dm_probe_cache.lock().await;
        // Age-based eviction on write keeps the cache bounded.
        cache.retain(|_, (_, fetched_at)| fetched_at.elapsed() < DM_PROBE_CACHE_TTL);
        cache.insert(room_id.to_owned(), (outcome, Instant::now()));
        outcome.unwrap_or(true)
    }

    /// Query the homeserver for the joined members of `room_id` and report
    /// whether at most one user besides this account is present. Returns
    /// `None` on any failure.
    async fn probe_room_is_dm(&self, room_id: &str) -> Option<bool> {
        let resolved = self.resolved.lock().await.clone()?;
        let url = self.api_url(&format!(
            "/_matrix/client/v3/rooms/{}/joined_members",
            percent_encode_path(room_id)
        ));
        // One deadline for the whole exchange (connect, request, headers,
        // body): a trickling homeserver must not stretch the stall past
        // `DM_PROBE_TIMEOUT` either.
        let body = tokio::time::timeout(DM_PROBE_TIMEOUT, async {
            let resp = match self
                .http
                .get(&url)
                .bearer_auth(&resolved.access_token)
                .send()
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    debug!(room_id, error = %e, "Matrix joined_members probe failed for mention gate");
                    return None;
                }
            };
            if !resp.status().is_success() {
                debug!(
                    room_id,
                    status = resp.status().as_u16(),
                    "Matrix joined_members probe returned non-success for mention gate"
                );
                return None;
            }
            match resp.json::<Value>().await {
                Ok(body) => Some(body),
                Err(e) => {
                    debug!(room_id, error = %e, "failed to parse joined_members for mention gate");
                    None
                }
            }
        })
        .await;
        let body: Value = match body {
            Ok(Some(body)) => body,
            Ok(None) => return None,
            Err(_) => {
                debug!(
                    room_id,
                    "Matrix joined_members probe timed out for mention gate"
                );
                return None;
            }
        };
        let joined = body.get("joined")?.as_object()?;
        let others = joined
            .keys()
            .filter(|id| !id.eq_ignore_ascii_case(&resolved.user_id))
            .count();
        Some(others <= 1)
    }

    /// Forward parsed messages to the bus, applying dedup and allowlists.
    async fn forward_messages(
        &self,
        messages: Vec<ParsedMessage>,
        inbound_tx: &mpsc::Sender<InboundMessage>,
    ) -> Result<()> {
        for msg in messages {
            if !self.room_allowed(&msg.room_id) {
                continue;
            }
            if !self.sender_allowed(&msg.sender) {
                continue;
            }
            if self.require_mention && !msg.mentioned_self && !is_slash_command(&msg.body) {
                debug!(
                    room_id = %msg.room_id,
                    sender = %msg.sender,
                    "Matrix message ignored; mention required"
                );
                continue;
            }
            // Mention-aware suppression: a group-room message that explicitly
            // mentions someone else is directed at them, not at the room —
            // stay silent even when `require_mention` is off, or every bot in
            // a multi-bot room would answer every addressed message. DMs are
            // exempt (a 1:1 keeps answering everything), as are slash
            // commands. Runs after the `require_mention` gate so a message
            // that gate would drop anyway never triggers a membership probe.
            if self.mention_policy == MatrixMentionPolicy::Strict
                && !msg.other_mentions.is_empty()
                && !msg.mentioned_self
                && !is_slash_command(&msg.body)
                && !self.is_direct_room(&msg.room_id).await
            {
                if self
                    .suppression_logged_rooms
                    .lock()
                    .await
                    .insert(msg.room_id.clone())
                {
                    info!(
                        room_id = %msg.room_id,
                        sender = %msg.sender,
                        policy = ?self.mention_policy,
                        mentions = ?msg.other_mentions,
                        "Matrix group message suppressed; explicitly mentions another user \
                         (first suppression in this room since startup)"
                    );
                }
                debug!(
                    room_id = %msg.room_id,
                    sender = %msg.sender,
                    "Matrix message ignored; explicitly mentions another user"
                );
                continue;
            }
            if let Some(event_id) = &msg.event_id {
                if self.dedup.is_duplicate(event_id) {
                    continue;
                }
            }
            let inbound = InboundMessage {
                channel: CHANNEL_NAME.into(),
                sender_id: msg.sender,
                chat_id: msg.room_id,
                content: msg.body,
                timestamp: Utc::now(),
                media: vec![],
                metadata: json!({}),
                message_id: msg.event_id,
                origin: octos_core::MessageOrigin::ExternalUser,
            };
            if inbound_tx.send(inbound).await.is_err() {
                return Err(eyre!("inbound channel closed"));
            }
        }
        Ok(())
    }
}

#[async_trait]
impl Channel for MatrixUserChannel {
    fn name(&self) -> &str {
        CHANNEL_NAME
    }

    fn max_message_length(&self) -> usize {
        65535
    }

    fn is_allowed(&self, sender_id: &str) -> bool {
        self.sender_allowed(sender_id)
    }

    async fn start(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        let resolved = self.resolve_login().await?;
        info!(
            user_id = %resolved.user_id,
            homeserver = %self.homeserver,
            "Matrix user channel authenticated"
        );
        {
            *self.resolved.lock().await = Some(resolved.clone());
        }
        let token = resolved.access_token.clone();
        let self_user_id = resolved.user_id.clone();

        // Initial sync (timeout=0): pick up the latest position and any pending
        // invites without replaying historical messages.
        let Some(mut since) = self.initial_sync_cursor(&token, &self_user_id).await? else {
            return Ok(());
        };

        let mut backoff = Duration::from_secs(1);
        while !self.shutdown.load(Ordering::Acquire) {
            match self
                .sync_once(&token, &self_user_id, Some(&since), SYNC_TIMEOUT_MS)
                .await
            {
                Ok(parsed) => {
                    if let Some(next_batch) = parsed.next_batch {
                        since = next_batch;
                    }
                    self.update_direct_rooms(parsed.direct_rooms).await;
                    self.join_allowed_invites(&token, parsed.invites).await;
                    self.forward_messages(parsed.messages, &inbound_tx).await?;
                    backoff = Duration::from_secs(1);
                }
                Err(e) => {
                    warn!(error = %e, "Matrix sync error; backing off");
                    if self.shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }

        Ok(())
    }

    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        let resolved = {
            self.resolved
                .lock()
                .await
                .clone()
                .ok_or_else(|| eyre!("Matrix user channel not started"))?
        };
        let txn_id = uuid::Uuid::now_v7().to_string();
        let url = self.api_url(&format!(
            "/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            percent_encode_path(&msg.chat_id),
            percent_encode_path(&txn_id),
        ));
        let formatted_body = markdown_to_matrix_html(&msg.content);
        let body = json!({
            "msgtype": MSGTYPE_TEXT,
            "body": msg.content,
            "format": "org.matrix.custom.html",
            "formatted_body": formatted_body,
        });
        let resp = self
            .http
            .put(&url)
            .bearer_auth(&resolved.access_token)
            .json(&body)
            .send()
            .await
            .wrap_err("failed to send Matrix message")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = matrix_error_body(resp).await;
            return Err(eyre!("Matrix send failed (status={status}): {text}"));
        }
        Ok(())
    }

    async fn send_typing(&self, chat_id: &str) -> Result<()> {
        let resolved = match self.resolved.lock().await.clone() {
            Some(r) => r,
            None => return Ok(()),
        };
        let url = self.api_url(&format!(
            "/_matrix/client/v3/rooms/{}/typing/{}",
            percent_encode_path(chat_id),
            percent_encode_path(&resolved.user_id),
        ));
        let body = json!({ "typing": true, "timeout": SYNC_TIMEOUT_MS });
        if let Err(e) = self
            .http
            .put(&url)
            .bearer_auth(&resolved.access_token)
            .json(&body)
            .send()
            .await
        {
            debug!(error = %e, "Matrix typing indicator failed");
        }
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.shutdown.store(true, Ordering::Release);
        let resolved = self.resolved.lock().await.take();
        if let Some(resolved) = resolved.filter(|r| r.logout_on_stop) {
            let url = self.api_url("/_matrix/client/v3/logout");
            match self
                .http
                .post(&url)
                .bearer_auth(&resolved.access_token)
                .json(&json!({}))
                .send()
                .await
            {
                Ok(resp) if !resp.status().is_success() => {
                    warn!(status = %resp.status(), "Matrix logout request returned non-success");
                }
                Ok(_) => {}
                Err(e) => {
                    warn!(error = %e, "Matrix logout request failed");
                }
            }
        }
        Ok(())
    }

    async fn health_check(&self) -> Result<ChannelHealth> {
        let resolved = match self.resolved.lock().await.clone() {
            Some(r) => r,
            None => return Ok(ChannelHealth::Unknown),
        };
        let url = self.api_url("/_matrix/client/v3/account/whoami");
        match self
            .http
            .get(&url)
            .bearer_auth(&resolved.access_token)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => Ok(ChannelHealth::Healthy),
            Ok(resp) => Ok(ChannelHealth::Down(format!("status={}", resp.status()))),
            Err(e) => Ok(ChannelHealth::Down(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_parse_text_message_from_join_timeline() {
        let payload = json!({
            "next_batch": "s2",
            "rooms": {
                "join": {
                    "!room:example.org": {
                        "timeline": {
                            "events": [
                                {
                                    "type": "m.room.message",
                                    "sender": "@alice:example.org",
                                    "event_id": "$evt1",
                                    "content": { "msgtype": "m.text", "body": "hello" }
                                }
                            ]
                        }
                    }
                }
            }
        });

        let parsed = parse_sync(&payload, "@bot:example.org");
        assert_eq!(parsed.next_batch.as_deref(), Some("s2"));
        assert_eq!(parsed.messages.len(), 1);
        let m = &parsed.messages[0];
        assert_eq!(m.room_id, "!room:example.org");
        assert_eq!(m.sender, "@alice:example.org");
        assert_eq!(m.body, "hello");
        assert_eq!(m.event_id.as_deref(), Some("$evt1"));
    }

    #[test]
    fn should_skip_own_messages() {
        let payload = json!({
            "rooms": { "join": { "!r:example.org": { "timeline": { "events": [
                { "type": "m.room.message", "sender": "@bot:example.org",
                  "content": { "msgtype": "m.text", "body": "from me" } }
            ] } } } }
        });
        let parsed = parse_sync(&payload, "@bot:example.org");
        assert!(parsed.messages.is_empty());
    }

    #[test]
    fn should_skip_non_text_and_empty_bodies() {
        let payload = json!({
            "rooms": { "join": { "!r:example.org": { "timeline": { "events": [
                { "type": "m.room.message", "sender": "@a:example.org",
                  "content": { "msgtype": "m.image", "body": "pic.png" } },
                { "type": "m.room.message", "sender": "@a:example.org",
                  "content": { "msgtype": "m.text", "body": "" } },
                { "type": "m.reaction", "sender": "@a:example.org",
                  "content": { "msgtype": "m.text", "body": "ignored" } }
            ] } } } }
        });
        let parsed = parse_sync(&payload, "@bot:example.org");
        assert!(parsed.messages.is_empty());
    }

    #[test]
    fn should_collect_invites() {
        let payload = json!({
            "rooms": { "invite": {
                "!a:example.org": {
                    "invite_state": {
                        "events": [
                            {
                                "type": "m.room.name",
                                "content": { "name": "Ops Room" }
                            },
                            {
                                "type": "m.room.canonical_alias",
                                "content": { "alias": "#ops:example.org" }
                            },
                            {
                                "type": "m.room.member",
                                "sender": "@alice:example.org",
                                "state_key": "@bot:example.org",
                                "event_id": "$invite1",
                                "content": { "membership": "invite" }
                            }
                        ]
                    }
                },
                "!b:example.org": {}
            } }
        });
        let parsed = parse_sync(&payload, "@bot:example.org");
        assert_eq!(parsed.invites.len(), 2);
        let invite = parsed
            .invites
            .iter()
            .find(|invite| invite.room_id == "!a:example.org")
            .expect("invite parsed");
        assert_eq!(invite.room_name.as_deref(), Some("Ops Room"));
        assert_eq!(invite.canonical_alias.as_deref(), Some("#ops:example.org"));
        assert_eq!(invite.inviter.as_deref(), Some("@alice:example.org"));
        assert_eq!(invite.membership_event_id.as_deref(), Some("$invite1"));
    }

    #[test]
    fn should_build_sync_path_with_since() {
        assert_eq!(sync_path(None, 0), "/_matrix/client/v3/sync?timeout=0");
        assert_eq!(
            sync_path(Some("s2"), 30000),
            "/_matrix/client/v3/sync?timeout=30000&since=s2"
        );
        // empty/whitespace since is ignored
        assert_eq!(
            sync_path(Some("  "), 100),
            "/_matrix/client/v3/sync?timeout=100"
        );
    }

    #[tokio::test]
    async fn initial_sync_retries_until_cursor_obtained() {
        use std::sync::atomic::AtomicUsize;

        use axum::Router;
        use axum::http::StatusCode;
        use axum::response::IntoResponse;
        use axum::routing::get;

        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_route = attempts.clone();
        let app = Router::new().route(
            "/_matrix/client/v3/sync",
            get(move || {
                let attempts = attempts_for_route.clone();
                async move {
                    let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                    if attempt == 0 {
                        (StatusCode::BAD_GATEWAY, "temporary").into_response()
                    } else {
                        axum::Json(json!({ "next_batch": "s2" })).into_response()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let shutdown = Arc::new(AtomicBool::new(false));
        let ch = MatrixUserChannel::new(
            &format!("http://{addr}"),
            Some("@bot:example.org".into()),
            Some("tok".into()),
            None,
            None,
            vec![],
            MatrixAutoJoin::Off,
            vec![],
            MatrixGroupPolicy::Open,
            false,
            MatrixMentionPolicy::Strict,
            vec![],
            shutdown,
        );

        let cursor = ch
            .initial_sync_cursor("tok", "@bot:example.org")
            .await
            .unwrap();

        assert_eq!(cursor.as_deref(), Some("s2"));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn matrix_runtime_http_client_does_not_follow_redirects() {
        use axum::Router;
        use axum::response::Redirect;
        use axum::routing::get;

        let app = Router::new()
            .route(
                "/redirect",
                get(|| async { Redirect::temporary("/target") }),
            )
            .route("/target", get(|| async { "target" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let resp = matrix_http_client()
            .get(format!("http://{addr}/redirect"))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
    }

    #[test]
    fn should_build_password_login_body() {
        let body = password_login_body("@u:example.org", "secret", Some("MyDevice"));
        assert_eq!(body["type"], "m.login.password");
        assert_eq!(body["identifier"]["type"], "m.id.user");
        assert_eq!(body["identifier"]["user"], "@u:example.org");
        assert_eq!(body["password"], "secret");
        assert_eq!(body["initial_device_display_name"], "MyDevice");

        let default_body = password_login_body("@u:example.org", "secret", None);
        assert_eq!(
            default_body["initial_device_display_name"],
            DEFAULT_DEVICE_NAME
        );
    }

    #[test]
    fn should_apply_room_allowlist() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let ch = MatrixUserChannel::new(
            "https://example.org",
            Some("@u:example.org".into()),
            Some("tok".into()),
            None,
            None,
            vec!["!allowed:example.org".into()],
            MatrixAutoJoin::Off,
            vec![],
            MatrixGroupPolicy::Allowlist,
            false,
            MatrixMentionPolicy::Strict,
            vec![],
            shutdown,
        );
        assert!(ch.room_allowed("!allowed:example.org"));
        assert!(!ch.room_allowed("!other:example.org"));

        let shutdown2 = Arc::new(AtomicBool::new(false));
        let open = MatrixUserChannel::new(
            "https://example.org",
            None,
            Some("tok".into()),
            None,
            None,
            vec![],
            MatrixAutoJoin::Off,
            vec![],
            MatrixGroupPolicy::Open,
            false,
            MatrixMentionPolicy::Strict,
            vec![],
            shutdown2,
        );
        assert!(open.room_allowed("!anything:example.org"));
    }

    #[test]
    fn should_apply_sender_allowlist() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let ch = MatrixUserChannel::new(
            "https://example.org",
            None,
            Some("tok".into()),
            None,
            None,
            vec![],
            MatrixAutoJoin::Off,
            vec![],
            MatrixGroupPolicy::Open,
            false,
            MatrixMentionPolicy::Strict,
            vec!["@alice:example.org".into()],
            shutdown,
        );
        assert!(ch.sender_allowed("@alice:example.org"));
        assert!(ch.is_allowed("@alice:example.org"));
        assert!(!ch.sender_allowed("@mallory:example.org"));
        assert!(!ch.is_allowed("@mallory:example.org"));

        let shutdown2 = Arc::new(AtomicBool::new(false));
        let open = MatrixUserChannel::new(
            "https://example.org",
            None,
            Some("tok".into()),
            None,
            None,
            vec![],
            MatrixAutoJoin::Off,
            vec![],
            MatrixGroupPolicy::Open,
            false,
            MatrixMentionPolicy::Strict,
            vec![],
            shutdown2,
        );
        assert!(open.sender_allowed("@anyone:example.org"));
    }

    #[tokio::test]
    async fn should_apply_auto_join_policy() {
        let shutdown = Arc::new(AtomicBool::new(false));
        let off = MatrixUserChannel::new(
            "https://example.org",
            None,
            Some("tok".into()),
            None,
            None,
            vec!["!allowed:example.org".into()],
            MatrixAutoJoin::Off,
            vec![],
            MatrixGroupPolicy::Allowlist,
            false,
            MatrixMentionPolicy::Strict,
            vec![],
            shutdown,
        );
        assert!(
            !off.auto_join_allowed("tok", "!allowed:example.org").await,
            "auto_join=off must reject even allowlisted room invites"
        );

        let shutdown2 = Arc::new(AtomicBool::new(false));
        let allowlist = MatrixUserChannel::new(
            "https://example.org",
            None,
            Some("tok".into()),
            None,
            None,
            vec!["!allowed:example.org".into()],
            MatrixAutoJoin::Allowlist,
            vec![],
            MatrixGroupPolicy::Allowlist,
            false,
            MatrixMentionPolicy::Strict,
            vec![],
            shutdown2,
        );
        assert!(
            allowlist
                .auto_join_allowed("tok", "!allowed:example.org")
                .await
        );
        assert!(
            !allowlist
                .auto_join_allowed("tok", "!other:example.org")
                .await
        );

        let shutdown3 = Arc::new(AtomicBool::new(false));
        let always = MatrixUserChannel::new(
            "https://example.org",
            None,
            Some("tok".into()),
            None,
            None,
            vec![],
            MatrixAutoJoin::Always,
            vec![],
            MatrixGroupPolicy::Open,
            false,
            MatrixMentionPolicy::Strict,
            vec![],
            shutdown3,
        );
        assert!(
            always
                .auto_join_allowed("tok", "!anything:example.org")
                .await
        );
    }

    #[test]
    fn should_persist_pending_invites() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = MatrixInviteStore::for_profile_data_dir(tmp.path());
        let invite = MatrixPendingInvite {
            channel_index: 2,
            room_id: "!room:example.org".into(),
            room_name: Some("Ops".into()),
            canonical_alias: Some("#ops:example.org".into()),
            inviter: Some("@alice:example.org".into()),
            membership_event_id: Some("$invite1".into()),
            received_at: Utc::now(),
            last_seen_at: Utc::now(),
            dismissed_at: None,
        };

        store.upsert(invite).unwrap();
        let listed = store.list(false).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].room_id, "!room:example.org");
        assert_eq!(listed[0].channel_index, 2);

        assert!(store.dismiss(2, "!room:example.org").unwrap());
        assert!(store.list(false).unwrap().is_empty());
        assert_eq!(store.list(true).unwrap().len(), 1);

        assert!(store.remove(2, "!room:example.org").unwrap());
        assert!(store.list(true).unwrap().is_empty());
    }

    /// Seed the login state `start()` would populate, so the DM probe can
    /// call the homeserver stub in forward-only tests.
    async fn seed_resolved(ch: &MatrixUserChannel) {
        *ch.resolved.lock().await = Some(ResolvedClient {
            access_token: "tok".into(),
            user_id: "@bot:example.org".into(),
            logout_on_stop: false,
        });
    }

    /// Reproduction for octos-org/octos#1547: with `require_mention: false`,
    /// a group-room message that explicitly mentions a DIFFERENT user (and not
    /// this bot) must stay unanswered. Drives the real HTTP sync + membership
    /// probe + forward path against a local homeserver stub.
    #[tokio::test]
    async fn should_stay_silent_when_group_message_mentions_another_user() {
        use axum::Router;
        use axum::routing::get;

        let app = Router::new()
            .route(
                "/_matrix/client/v3/sync",
                get(|| async {
                    axum::Json(json!({
                        "next_batch": "s2",
                        "rooms": { "join": { "!room:example.org": { "timeline": { "events": [
                            {
                                "type": "m.room.message",
                                "sender": "@alice:example.org",
                                "event_id": "$evt1",
                                "content": {
                                    "msgtype": "m.text",
                                    "body": "@otherbot:example.org 你是谁",
                                    "m.mentions": { "user_ids": ["@otherbot:example.org"] }
                                }
                            }
                        ] } } } }
                    }))
                }),
            )
            // The room ID is percent-encoded in the request path, so a
            // literal route would not match; answer any joined_members probe.
            .fallback(get(|| async {
                axum::Json(json!({ "joined": {
                    "@bot:example.org": {},
                    "@alice:example.org": {},
                    "@otherbot:example.org": {}
                } }))
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let shutdown = Arc::new(AtomicBool::new(false));
        let ch = MatrixUserChannel::new(
            &format!("http://{addr}"),
            Some("@bot:example.org".into()),
            Some("tok".into()),
            None,
            None,
            vec![],
            MatrixAutoJoin::Off,
            vec![],
            MatrixGroupPolicy::Open,
            false,
            MatrixMentionPolicy::Strict,
            vec![],
            shutdown,
        );
        seed_resolved(&ch).await;

        let parsed = ch
            .sync_once("tok", "@bot:example.org", None, 0)
            .await
            .unwrap();
        assert_eq!(parsed.messages.len(), 1);
        let (tx, mut rx) = mpsc::channel(8);
        ch.forward_messages(parsed.messages, &tx).await.unwrap();
        assert!(
            rx.try_recv().is_err(),
            "message explicitly mentioning another user must not be forwarded"
        );
    }

    /// The `m.direct` account data carried by a sync marks DM rooms; mention
    /// suppression must not apply inside them (a 1:1 keeps answering). The
    /// membership probe would report a group here, so only `m.direct` can
    /// exempt the room — pinning that the account-data path is what fires.
    #[tokio::test]
    async fn should_answer_mention_of_other_user_inside_direct_room() {
        use axum::Router;
        use axum::routing::get;

        let app = Router::new()
            .route(
                "/_matrix/client/v3/sync",
                get(|| async {
                    axum::Json(json!({
                        "next_batch": "s2",
                        "account_data": { "events": [
                            {
                                "type": "m.direct",
                                "content": { "@bot:example.org": ["!dm:example.org"] }
                            }
                        ] },
                        "rooms": { "join": { "!dm:example.org": { "timeline": { "events": [
                            {
                                "type": "m.room.message",
                                "sender": "@alice:example.org",
                                "event_id": "$evt1",
                                "content": {
                                    "msgtype": "m.text",
                                    "body": "can you ask @carol:example.org about this?",
                                    "m.mentions": { "user_ids": ["@carol:example.org"] }
                                }
                            }
                        ] } } } }
                    }))
                }),
            )
            .fallback(get(|| async {
                axum::Json(json!({ "joined": {
                    "@bot:example.org": {},
                    "@alice:example.org": {},
                    "@carol:example.org": {}
                } }))
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let shutdown = Arc::new(AtomicBool::new(false));
        let ch = MatrixUserChannel::new(
            &format!("http://{addr}"),
            Some("@bot:example.org".into()),
            Some("tok".into()),
            None,
            None,
            vec![],
            MatrixAutoJoin::Off,
            vec![],
            MatrixGroupPolicy::Open,
            false,
            MatrixMentionPolicy::Strict,
            vec![],
            shutdown,
        );
        seed_resolved(&ch).await;

        let parsed = ch
            .sync_once("tok", "@bot:example.org", None, 0)
            .await
            .unwrap();
        ch.update_direct_rooms(parsed.direct_rooms).await;
        let (tx, mut rx) = mpsc::channel(8);
        ch.forward_messages(parsed.messages, &tx).await.unwrap();
        let inbound = rx.try_recv().expect("DM message must be forwarded");
        assert_eq!(inbound.chat_id, "!dm:example.org");
    }

    /// Without `m.direct` (the common case — nobody ever writes the bot's own
    /// `m.direct`), a 1:1 is detected by probing the room's joined members.
    #[tokio::test]
    async fn should_answer_mention_of_other_user_when_membership_probe_finds_dm() {
        use axum::Router;
        use axum::routing::get;

        let app = Router::new()
            .route(
                "/_matrix/client/v3/sync",
                get(|| async {
                    axum::Json(json!({
                        "next_batch": "s2",
                        "rooms": { "join": { "!dm:example.org": { "timeline": { "events": [
                            {
                                "type": "m.room.message",
                                "sender": "@alice:example.org",
                                "event_id": "$evt1",
                                "content": {
                                    "msgtype": "m.text",
                                    "body": "can you ask @carol:example.org about this?",
                                    "m.mentions": { "user_ids": ["@carol:example.org"] }
                                }
                            }
                        ] } } } }
                    }))
                }),
            )
            .fallback(get(|| async {
                axum::Json(json!({ "joined": {
                    "@bot:example.org": {},
                    "@alice:example.org": {}
                } }))
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let shutdown = Arc::new(AtomicBool::new(false));
        let ch = MatrixUserChannel::new(
            &format!("http://{addr}"),
            Some("@bot:example.org".into()),
            Some("tok".into()),
            None,
            None,
            vec![],
            MatrixAutoJoin::Off,
            vec![],
            MatrixGroupPolicy::Open,
            false,
            MatrixMentionPolicy::Strict,
            vec![],
            shutdown,
        );
        seed_resolved(&ch).await;

        let parsed = ch
            .sync_once("tok", "@bot:example.org", None, 0)
            .await
            .unwrap();
        ch.update_direct_rooms(parsed.direct_rooms).await;
        let (tx, mut rx) = mpsc::channel(8);
        ch.forward_messages(parsed.messages, &tx).await.unwrap();
        let inbound = rx
            .try_recv()
            .expect("1:1 message must be forwarded after the membership probe");
        assert_eq!(inbound.chat_id, "!dm:example.org");
        // The probe result is cached, so a second candidate does not re-probe.
        assert!(
            ch.dm_probe_cache
                .lock()
                .await
                .contains_key("!dm:example.org")
        );
    }

    /// When the membership probe fails, the gate fails toward the pre-#1547
    /// behaviour (answer) instead of extending its reach on an error.
    #[tokio::test]
    async fn should_answer_mention_of_other_user_when_membership_probe_fails() {
        use axum::Router;
        use axum::http::StatusCode;
        use axum::routing::get;

        let app = Router::new()
            .route(
                "/_matrix/client/v3/sync",
                get(|| async {
                    axum::Json(json!({
                        "next_batch": "s2",
                        "rooms": { "join": { "!room:example.org": { "timeline": { "events": [
                            {
                                "type": "m.room.message",
                                "sender": "@alice:example.org",
                                "event_id": "$evt1",
                                "content": {
                                    "msgtype": "m.text",
                                    "body": "@otherbot:example.org ping",
                                    "m.mentions": { "user_ids": ["@otherbot:example.org"] }
                                }
                            }
                        ] } } } }
                    }))
                }),
            )
            .fallback(get(|| async { StatusCode::FORBIDDEN }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let shutdown = Arc::new(AtomicBool::new(false));
        let ch = MatrixUserChannel::new(
            &format!("http://{addr}"),
            Some("@bot:example.org".into()),
            Some("tok".into()),
            None,
            None,
            vec![],
            MatrixAutoJoin::Off,
            vec![],
            MatrixGroupPolicy::Open,
            false,
            MatrixMentionPolicy::Strict,
            vec![],
            shutdown,
        );
        seed_resolved(&ch).await;

        let parsed = ch
            .sync_once("tok", "@bot:example.org", None, 0)
            .await
            .unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        ch.forward_messages(parsed.messages, &tx).await.unwrap();
        assert!(
            rx.try_recv().is_ok(),
            "probe failure must not silence the channel"
        );
    }

    /// A homeserver that accepts the probe but never answers must not stall
    /// the forward loop: after [`DM_PROBE_TIMEOUT`] the probe fails open and
    /// the candidate message is answered (paused clock, so this runs fast).
    #[tokio::test(start_paused = true)]
    async fn should_fail_open_when_membership_probe_hangs() {
        use axum::Router;
        use axum::routing::get;

        let app = Router::new()
            .route(
                "/_matrix/client/v3/sync",
                get(|| async {
                    axum::Json(json!({
                        "next_batch": "s2",
                        "rooms": { "join": { "!room:example.org": { "timeline": { "events": [
                            {
                                "type": "m.room.message",
                                "sender": "@alice:example.org",
                                "event_id": "$evt1",
                                "content": {
                                    "msgtype": "m.text",
                                    "body": "@otherbot:example.org ping",
                                    "m.mentions": { "user_ids": ["@otherbot:example.org"] }
                                }
                            }
                        ] } } } }
                    }))
                }),
            )
            // The probe route never responds, simulating a hung homeserver.
            .fallback(get(std::future::pending::<axum::Json<Value>>));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let shutdown = Arc::new(AtomicBool::new(false));
        let ch = MatrixUserChannel::new(
            &format!("http://{addr}"),
            Some("@bot:example.org".into()),
            Some("tok".into()),
            None,
            None,
            vec![],
            MatrixAutoJoin::Off,
            vec![],
            MatrixGroupPolicy::Open,
            false,
            MatrixMentionPolicy::Strict,
            vec![],
            shutdown,
        );
        seed_resolved(&ch).await;

        let parsed = ch
            .sync_once("tok", "@bot:example.org", None, 0)
            .await
            .unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        // `tokio::time::Instant` follows the paused virtual clock.
        let started = tokio::time::Instant::now();
        ch.forward_messages(parsed.messages, &tx).await.unwrap();
        assert!(
            rx.try_recv().is_ok(),
            "a hung probe must fail open after DM_PROBE_TIMEOUT"
        );
        // The paused clock jumped straight to the probe deadline: the stall
        // is exactly one DM_PROBE_TIMEOUT, not more, not less.
        assert_eq!(started.elapsed(), DM_PROBE_TIMEOUT);
        // The hang is cached as a failed probe, like any other failure.
        assert!(
            ch.dm_probe_cache
                .lock()
                .await
                .contains_key("!room:example.org")
        );
    }

    /// The first suppression per room is logged once at info (with the
    /// policy and the mention set) so operators discover the behaviour
    /// change from their logs; repeats in the same room stay at debug.
    #[tokio::test]
    async fn should_log_first_suppression_per_room_at_info() {
        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .without_time()
            .with_writer(capture.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let ch = gate_test_channel(false, MatrixMentionPolicy::Strict);
        ch.dm_probe_cache
            .lock()
            .await
            .insert("!room:example.org".into(), (Some(false), Instant::now()));
        let (tx, mut rx) = mpsc::channel(8);
        ch.forward_messages(
            vec![
                gate_test_message("$e1", false, true),
                gate_test_message("$e2", false, true),
            ],
            &tx,
        )
        .await
        .unwrap();
        assert!(rx.try_recv().is_err());

        let logs = capture.contents();
        let notices = logs.matches("first suppression in this room").count();
        assert_eq!(
            notices, 1,
            "exactly one info notice per room, got logs: {logs}"
        );
        assert!(
            logs.contains("Strict"),
            "notice must name the policy: {logs}"
        );
        assert!(
            logs.contains("@other:example.org"),
            "notice must include the mention set: {logs}"
        );
    }

    /// Captures `tracing` output so log-emitting behaviour (the once-per-room
    /// suppression notice) can be asserted.
    #[derive(Clone, Default)]
    struct LogCapture {
        buf: Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl LogCapture {
        fn contents(&self) -> String {
            String::from_utf8_lossy(&self.buf.lock().unwrap()).into_owned()
        }
    }

    impl std::io::Write for LogCapture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.buf.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
        type Writer = LogCapture;

        fn make_writer(&self) -> Self::Writer {
            self.clone()
        }
    }

    fn gate_test_channel(
        require_mention: bool,
        mention_policy: MatrixMentionPolicy,
    ) -> MatrixUserChannel {
        MatrixUserChannel::new(
            "https://example.org",
            Some("@bot:example.org".into()),
            Some("tok".into()),
            None,
            None,
            vec![],
            MatrixAutoJoin::Off,
            vec![],
            MatrixGroupPolicy::Open,
            require_mention,
            mention_policy,
            vec![],
            Arc::new(AtomicBool::new(false)),
        )
    }

    fn gate_test_message(
        event_id: &str,
        mentioned_self: bool,
        mentions_other: bool,
    ) -> ParsedMessage {
        ParsedMessage {
            room_id: "!room:example.org".into(),
            sender: "@alice:example.org".into(),
            body: "hello".into(),
            event_id: Some(event_id.into()),
            mentioned_self,
            other_mentions: if mentions_other {
                vec!["@other:example.org".to_owned()]
            } else {
                vec![]
            },
        }
    }

    #[tokio::test]
    async fn should_forward_other_mention_when_mention_policy_open() {
        let ch = gate_test_channel(false, MatrixMentionPolicy::Open);
        let (tx, mut rx) = mpsc::channel(8);
        ch.forward_messages(vec![gate_test_message("$e1", false, true)], &tx)
            .await
            .unwrap();
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn should_answer_message_mentioning_self_and_other() {
        let ch = gate_test_channel(false, MatrixMentionPolicy::Strict);
        let (tx, mut rx) = mpsc::channel(8);
        ch.forward_messages(vec![gate_test_message("$e1", true, true)], &tx)
            .await
            .unwrap();
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn should_still_forward_unaddressed_group_message() {
        let ch = gate_test_channel(false, MatrixMentionPolicy::Strict);
        let (tx, mut rx) = mpsc::channel(8);
        ch.forward_messages(vec![gate_test_message("$e1", false, false)], &tx)
            .await
            .unwrap();
        assert!(rx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn should_suppress_other_mention_when_room_known_to_be_group() {
        let ch = gate_test_channel(false, MatrixMentionPolicy::Strict);
        ch.dm_probe_cache
            .lock()
            .await
            .insert("!room:example.org".into(), (Some(false), Instant::now()));
        let (tx, mut rx) = mpsc::channel(8);
        ch.forward_messages(vec![gate_test_message("$e1", false, true)], &tx)
            .await
            .unwrap();
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn should_drop_other_mention_with_require_mention_enabled_too() {
        let ch = gate_test_channel(true, MatrixMentionPolicy::Strict);
        ch.dm_probe_cache
            .lock()
            .await
            .insert("!room:example.org".into(), (Some(false), Instant::now()));
        let (tx, mut rx) = mpsc::channel(8);
        ch.forward_messages(vec![gate_test_message("$e1", false, true)], &tx)
            .await
            .unwrap();
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn should_preserve_and_clear_direct_rooms_on_account_data_updates() {
        let ch = gate_test_channel(false, MatrixMentionPolicy::Strict);
        ch.update_direct_rooms(Some(vec!["!dm:example.org".into()]))
            .await;
        assert!(ch.direct_rooms.lock().await.contains("!dm:example.org"));
        // A sync without m.direct keeps the previous set.
        ch.update_direct_rooms(None).await;
        assert!(ch.direct_rooms.lock().await.contains("!dm:example.org"));
        // A sync with an empty m.direct map clears it.
        ch.update_direct_rooms(Some(vec![])).await;
        assert!(ch.direct_rooms.lock().await.is_empty());
    }

    #[test]
    fn should_not_treat_reply_fallback_mentions_as_addressing() {
        // A rich reply to Bob: clients auto-include Bob in `m.mentions` and
        // embed his pill inside the <mx-reply> fallback. The reply text
        // addresses the bot by name — it must NOT be treated as "mentions
        // someone else".
        let reply_to_bob = json!({
            "m.mentions": { "user_ids": ["@bob:example.org"] },
            "formatted_body": "<mx-reply><blockquote><a href=\"https://matrix.to/#/@bob:example.org\">Bob</a>: see this</blockquote></mx-reply>bot, summarize this thread",
            "m.relates_to": { "m.in_reply_to": { "event_id": "$orig" } }
        });
        let body = "> <@bob:example.org> see this\n\nbot, summarize this thread";
        assert!(content_other_mentions(&reply_to_bob, body, "@bot:example.org").is_empty());

        // A reply that ALSO explicitly mentions Carol still counts.
        let mut with_carol = reply_to_bob.clone();
        with_carol["m.mentions"] =
            json!({ "user_ids": ["@bob:example.org", "@carol:example.org"] });
        assert!(!content_other_mentions(&with_carol, body, "@bot:example.org").is_empty());

        // A pill AFTER the </mx-reply> fallback is a real mention.
        let pill_after_fallback = json!({
            "formatted_body": "<mx-reply><blockquote><a href=\"https://matrix.to/#/@bob:example.org\">Bob</a></blockquote></mx-reply>ask <a href=\"https://matrix.to/#/@carol:example.org\">Carol</a>",
            "m.relates_to": { "m.in_reply_to": { "event_id": "$orig" } }
        });
        assert!(!content_other_mentions(&pill_after_fallback, body, "@bot:example.org").is_empty());

        // A hand-typed MXID in the reply text (after the fallback) counts.
        let typed = json!({
            "m.relates_to": { "m.in_reply_to": { "event_id": "$orig" } }
        });
        assert!(
            !content_other_mentions(
                &typed,
                "> <@bob:example.org> see this\n\n@carol:example.org what do you think?",
                "@bot:example.org"
            )
            .is_empty()
        );
    }

    #[test]
    fn should_detect_mentions_other_with_edge_mxid_shapes() {
        // `+` is a valid localpart byte.
        assert!(
            !text_other_mentions("@user+device:example.org hi", "@bot:example.org", None)
                .is_empty()
        );
        // Self-comparison ignores ASCII case.
        assert!(text_other_mentions("@BOT:EXAMPLE.ORG hi", "@bot:example.org", None).is_empty());
        // …but a differently-cased OTHER user still counts.
        assert!(!text_other_mentions("@CAROL:example.org hi", "@bot:example.org", None).is_empty());
        // A port suffix is part of the server name.
        assert!(
            !text_other_mentions("@carol:localhost:8448 hi", "@bot:example.org", None).is_empty()
        );
        // The replied-to sender does not count, even hand-typed.
        assert!(
            text_other_mentions(
                "@bob:example.org agreed?",
                "@bot:example.org",
                Some("@bob:example.org")
            )
            .is_empty()
        );
    }

    #[test]
    fn should_detect_mentions_other_from_structured_mentions() {
        let other = json!({ "m.mentions": { "user_ids": ["@other:example.org"] } });
        assert!(!content_other_mentions(&other, "hi", "@bot:example.org").is_empty());

        let only_self = json!({ "m.mentions": { "user_ids": ["@bot:example.org"] } });
        assert!(content_other_mentions(&only_self, "hi", "@bot:example.org").is_empty());

        let self_and_other =
            json!({ "m.mentions": { "user_ids": ["@bot:example.org", "@other:example.org"] } });
        assert!(!content_other_mentions(&self_and_other, "hi", "@bot:example.org").is_empty());

        let empty = json!({ "m.mentions": { "user_ids": [] } });
        assert!(content_other_mentions(&empty, "hi", "@bot:example.org").is_empty());
    }

    #[test]
    fn should_detect_mentions_other_from_matrix_to_pill() {
        let pill_other = json!({
            "formatted_body": "<a href=\"https://matrix.to/#/@other:example.org\">other</a> hi"
        });
        assert!(!content_other_mentions(&pill_other, "hi", "@bot:example.org").is_empty());

        let pill_self = json!({
            "formatted_body": "<a href=\"https://matrix.to/#/@bot:example.org\">bot</a> hi"
        });
        assert!(content_other_mentions(&pill_self, "hi", "@bot:example.org").is_empty());
    }

    #[test]
    fn should_detect_mentions_other_from_plain_text_mxid() {
        assert!(
            !text_other_mentions("@otherbot:example.org 你是谁", "@bot:example.org", None)
                .is_empty()
        );
        assert!(
            !text_other_mentions("hi @otherbot:example.org!", "@bot:example.org", None).is_empty()
        );
        // A trailing sentence dot is not part of the MXID.
        assert!(
            text_other_mentions("thanks @bot:example.org.", "@bot:example.org", None).is_empty()
        );
        // Only the bot itself is mentioned.
        assert!(text_other_mentions("@bot:example.org hi", "@bot:example.org", None).is_empty());
        // Email addresses and casual "@name:" text are not MXID mentions.
        assert!(
            text_other_mentions("write to user@example.com: soon", "@bot:example.org", None)
                .is_empty()
        );
        assert!(text_other_mentions("@alice: hi", "@bot:example.org", None).is_empty());
    }

    #[test]
    fn should_parse_direct_rooms_from_account_data() {
        let payload = json!({
            "account_data": { "events": [
                { "type": "m.direct", "content": {
                    "@alice:example.org": ["!dm1:example.org", "!dm2:example.org"],
                    "@bob:example.org": ["!dm3:example.org"]
                } }
            ] }
        });
        assert_eq!(
            parse_direct_rooms(&payload),
            Some(vec![
                "!dm1:example.org".to_string(),
                "!dm2:example.org".to_string(),
                "!dm3:example.org".to_string()
            ])
        );

        // No account data at all -> no update.
        assert_eq!(parse_direct_rooms(&json!({ "rooms": {} })), None);
        // Account data without an m.direct event -> no update.
        assert_eq!(
            parse_direct_rooms(&json!({ "account_data": { "events": [
                { "type": "m.fully_read", "content": {} }
            ] } })),
            None
        );
        // Present but empty -> the DM set is cleared.
        assert_eq!(
            parse_direct_rooms(&json!({ "account_data": { "events": [
                { "type": "m.direct", "content": {} }
            ] } })),
            Some(vec![])
        );
    }
}
