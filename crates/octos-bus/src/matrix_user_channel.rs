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

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

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
const MSGTYPE_TEXT: &str = "m.text";
const MATRIX_INVITES_FILE: &str = "matrix-invites.json";
/// Long-poll timeout for the `/sync` request (server holds the connection open
/// until an event arrives or this elapses).
const SYNC_TIMEOUT_MS: u64 = 30_000;
const MATRIX_ERROR_BODY_MAX_BYTES: usize = 2048;
const DEFAULT_DEVICE_NAME: &str = "octos";

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
                messages.push(ParsedMessage {
                    room_id: room_id.clone(),
                    sender: sender.to_owned(),
                    body: body.to_owned(),
                    event_id,
                    mentioned_self,
                });
            }
        }
    }

    ParsedSync {
        next_batch,
        invites,
        messages,
    }
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
    channel_index: usize,
    invite_store: Option<MatrixInviteStore>,
    /// Optional sender allowlist. Empty means any sender in an allowed room.
    allowed_senders: HashSet<String>,
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
            channel_index: 0,
            invite_store: None,
            allowed_senders: allowed_senders
                .into_iter()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect(),
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
}
