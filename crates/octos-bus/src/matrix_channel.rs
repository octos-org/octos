//! Matrix Appservice channel.
//!
//! Implements the Matrix Application Service API, receiving events from a
//! homeserver (e.g. Palpo) via `PUT /_matrix/app/v1/transactions/{txn_id}` and
//! sending messages via the Client-Server API with appservice identity assertion.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, put};
use chrono::Utc;
use eyre::{Result, WrapErr};
use octos_core::{InboundMessage, METADATA_SENDER_USER_ID, OutboundMessage};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, RwLock, mpsc};
use tracing::{debug, info, warn};

use crate::channel::{Channel, ChannelHealth};
use crate::dedup::MessageDedup;
use crate::markdown_html::markdown_to_matrix_html;

// ── Matrix event type constants ──────────────────────────────────────────────

const CHANNEL_NAME: &str = "matrix";
const EVENT_ROOM_MESSAGE: &str = "m.room.message";
const EVENT_ROOM_MEMBER: &str = "m.room.member";
const MSGTYPE_TEXT: &str = "m.text";
const MSGTYPE_IMAGE: &str = "m.image";
const MSGTYPE_AUDIO: &str = "m.audio";
const MSGTYPE_VIDEO: &str = "m.video";
const MSGTYPE_FILE: &str = "m.file";
const MEMBERSHIP_INVITE: &str = "invite";
const REL_TYPE_REPLACE: &str = "m.replace";
const LIVE_MARKER: &str = "org.matrix.msc4357.live";
const HTML_FORMAT: &str = "org.matrix.custom.html";
const METADATA_TARGET_PROFILE_ID: &str = "target_profile_id";
const METADATA_TARGET_MATRIX_USER_ID: &str = "target_matrix_user_id";
const CONTENT_APP: &str = "org.octos.app";
const CONTENT_ACTIONS: &str = "org.octos.actions";
const CONTENT_ACTION_RESPONSE: &str = "org.octos.action_response";
const CONTENT_APPROVAL_REQUEST: &str = "org.octos.approval_request";
const CONTENT_APPROVAL_RESPONSE: &str = "org.octos.approval_response";
const CONTENT_TARGET_USER_ID: &str = "org.octos.target_user_id";
const CONTENT_TARGET_USER_ID_LEGACY: &str = "target_user_id";
const CONTENT_BROADCAST_TARGETS: &str = "org.octos.broadcast_targets";
#[cfg(not(test))]
const MAX_EVENT_SENDER_CACHE: usize = 2048;
#[cfg(test)]
const MAX_EVENT_SENDER_CACHE: usize = 4;
/// Upper bound on child bots a single `/allbots` broadcast may fan out to.
const MAX_ALLBOTS_TARGETS: usize = 8;

// ── Bot Manager trait ────────────────────────────────────────────────────────

/// Abstraction for bot lifecycle management via slash commands.
///
/// Implemented by the gateway layer which has access to `ProfileStore` and
/// `MatrixChannel`. Called from `handle_transaction` when a slash command is
/// detected, **before** messages reach the LLM agent.
#[async_trait]
pub trait BotManager: Send + Sync {
    /// Create a new bot. Returns a human-readable status message for the room.
    async fn create_bot(
        &self,
        username: &str,
        name: &str,
        system_prompt: Option<&str>,
        sender: &str,
        visibility: BotVisibility,
    ) -> Result<String>;

    /// Delete a bot by Matrix user ID. Returns a status message.
    async fn delete_bot(&self, matrix_user_id: &str, sender: &str) -> Result<String>;

    /// List all registered bots. Returns a formatted list.
    async fn list_bots(&self, sender: &str) -> Result<String>;

    /// Create a natural-language schedule for the current room context.
    async fn schedule_bot_task(&self, request: &str, sender: &str, room_id: &str)
    -> Result<String>;

    /// List schedule jobs visible to the current room context.
    async fn list_schedules(&self, sender: &str, room_id: &str) -> Result<String>;

    /// Remove a schedule job visible to the current room context.
    async fn unschedule_bot_task(
        &self,
        job_id: &str,
        sender: &str,
        room_id: &str,
    ) -> Result<String>;
}

// ── Bot Router ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BotEntry {
    pub profile_id: String,
    pub owner: String,
    pub visibility: BotVisibility,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BotVisibility {
    Public,
    Private,
}

/// Routes Matrix virtual user IDs to octos profile IDs.
/// Thread-safe, supports dynamic registration/unregistration.
///
/// Also tracks room → bot mappings for DM routing: when a bot is invited to a
/// room, `add_room_bot()` records the mapping so incoming messages in that room
/// can be routed to the correct profile without requiring an @mention.
pub struct BotRouter {
    routes: Arc<RwLock<HashMap<String, BotEntry>>>, // matrix_user_id -> metadata
    room_bots: Arc<RwLock<HashMap<String, HashSet<String>>>>, // room_id -> profile_ids
    persist_path: Option<PathBuf>,
    room_persist_path: Option<PathBuf>,
    update_lock: Arc<Mutex<()>>,
}

impl BotRouter {
    /// Create a new `BotRouter`, optionally loading persisted routes from `persist_path`.
    ///
    /// When `persist_path` is provided, also loads room-bot mappings from a
    /// sibling file (`matrix-bot-room-map.json` in the same directory).
    pub fn new(persist_path: Option<PathBuf>) -> Self {
        let routes = persist_path.as_deref().map(Self::load).unwrap_or_default();
        let room_persist_path = persist_path
            .as_deref()
            .and_then(|p| p.parent())
            .map(|dir| dir.join("matrix-bot-room-map.json"));
        let room_bots = room_persist_path
            .as_deref()
            .map(Self::load_rooms)
            .unwrap_or_default();
        Self {
            routes: Arc::new(RwLock::new(routes)),
            room_bots: Arc::new(RwLock::new(room_bots)),
            persist_path,
            room_persist_path,
            update_lock: Arc::new(Mutex::new(())),
        }
    }

    /// Register a mapping from a Matrix user ID to a profile ID.
    /// Persists the updated mapping to disk if a persist path is configured.
    pub async fn register(&self, matrix_user_id: &str, profile_id: &str) -> Result<()> {
        self.register_entry(matrix_user_id, profile_id, "", BotVisibility::Public)
            .await
    }

    pub async fn register_entry(
        &self,
        matrix_user_id: &str,
        profile_id: &str,
        owner: &str,
        visibility: BotVisibility,
    ) -> Result<()> {
        let _guard = self.update_lock.lock().await;
        let mut next_routes = self.routes.read().await.clone();
        next_routes.insert(
            matrix_user_id.to_string(),
            BotEntry {
                profile_id: profile_id.to_string(),
                owner: owner.to_string(),
                visibility,
            },
        );
        self.persist(&next_routes)?;
        let mut routes = self.routes.write().await;
        *routes = next_routes;
        Ok(())
    }

    /// Remove the mapping for a Matrix user ID.
    /// Persists the updated mapping to disk if a persist path is configured.
    pub async fn unregister(&self, matrix_user_id: &str) -> Result<()> {
        let _guard = self.update_lock.lock().await;
        let mut next_routes = self.routes.read().await.clone();
        next_routes.remove(matrix_user_id);
        self.persist(&next_routes)?;
        let mut routes = self.routes.write().await;
        *routes = next_routes;
        Ok(())
    }

    /// Look up the profile ID for a given Matrix user ID.
    pub async fn route(&self, matrix_user_id: &str) -> Option<String> {
        let routes = self.routes.read().await;
        routes
            .get(matrix_user_id)
            .map(|entry| entry.profile_id.clone())
    }

    pub async fn get_entry(&self, matrix_user_id: &str) -> Option<BotEntry> {
        let routes = self.routes.read().await;
        routes.get(matrix_user_id).cloned()
    }

    /// Reverse lookup: find the Matrix user ID mapped to a given profile ID.
    pub async fn reverse_route(&self, profile_id: &str) -> Option<String> {
        let routes = self.routes.read().await;
        routes
            .iter()
            .find(|(_, entry)| entry.profile_id.as_str() == profile_id)
            .map(|(uid, _)| uid.clone())
    }

    /// Load routes from a JSON file. Returns an empty map on any error.
    fn load(path: &std::path::Path) -> HashMap<String, BotEntry> {
        let data = match std::fs::read_to_string(path) {
            Ok(data) => data,
            Err(_) => return HashMap::new(),
        };
        let raw: HashMap<String, Value> = match serde_json::from_str(&data) {
            Ok(raw) => raw,
            Err(_) => return HashMap::new(),
        };
        raw.into_iter()
            .filter_map(|(matrix_user_id, value)| {
                if let Some(profile_id) = value.as_str() {
                    Some((
                        matrix_user_id,
                        BotEntry {
                            profile_id: profile_id.to_string(),
                            owner: String::new(),
                            visibility: BotVisibility::Public,
                        },
                    ))
                } else {
                    serde_json::from_value(value)
                        .ok()
                        .map(|entry| (matrix_user_id, entry))
                }
            })
            .collect()
    }

    /// Find a profile ID by scanning message text for any registered bot mention.
    pub async fn route_by_mention(&self, text: &str) -> Option<String> {
        let routes = self.routes.read().await;
        for (bot_user_id, entry) in routes.iter() {
            if contains_exact_matrix_user_id_mention(text, bot_user_id) {
                return Some(entry.profile_id.clone());
            }
        }
        None
    }

    /// Route by room: returns the profile ID if exactly one bot is in this room.
    /// Used for DM routing where the user messages a bot directly without @mention.
    pub async fn route_by_room(&self, room_id: &str) -> Option<String> {
        let room_bots = self.room_bots.read().await;
        let profiles = room_bots.get(room_id)?;
        if profiles.len() == 1 {
            profiles.iter().next().cloned()
        } else {
            None
        }
    }

    /// Record that a bot (by profile_id) is in a room.
    /// Called when a bot virtual user is invited to and joins a room.
    pub async fn add_room_bot(&self, room_id: &str, profile_id: &str) -> Result<()> {
        let _guard = self.update_lock.lock().await;
        let mut next = self.room_bots.read().await.clone();
        next.entry(room_id.to_string())
            .or_default()
            .insert(profile_id.to_string());
        self.persist_rooms(&next)?;
        let mut room_bots = self.room_bots.write().await;
        *room_bots = next;
        Ok(())
    }

    /// Whether the given profile (child bot) is bound to the given room.
    ///
    /// Server-side authority for `/allbots`: a broadcast may only reach bots
    /// actually bound to the originating room, regardless of what
    /// `org.octos.broadcast_targets` a (possibly forged) event claims.
    pub async fn is_profile_in_room(&self, room_id: &str, profile_id: &str) -> bool {
        let room_bots = self.room_bots.read().await;
        room_bots
            .get(room_id)
            .is_some_and(|profiles| profiles.contains(profile_id))
    }

    /// Return all room IDs that a given profile is in.
    pub async fn rooms_for_profile(&self, profile_id: &str) -> Vec<String> {
        let room_bots = self.room_bots.read().await;
        room_bots
            .iter()
            .filter(|(_, profiles)| profiles.contains(profile_id))
            .map(|(room_id, _)| room_id.clone())
            .collect()
    }

    /// Remove a bot from all rooms. Called when a bot is unregistered.
    pub async fn remove_bot_from_rooms(&self, profile_id: &str) -> Result<()> {
        let _guard = self.update_lock.lock().await;
        let mut next = self.room_bots.read().await.clone();
        next.values_mut().for_each(|set| {
            set.remove(profile_id);
        });
        next.retain(|_, set| !set.is_empty());
        self.persist_rooms(&next)?;
        let mut room_bots = self.room_bots.write().await;
        *room_bots = next;
        Ok(())
    }

    /// Reload routes and room-bot mappings from disk, replacing in-memory state.
    /// Called by the `/_octos/reload-bots` endpoint after CLI creates or deletes a bot.
    pub async fn reload(&self) -> Result<()> {
        let _guard = self.update_lock.lock().await;
        if let Some(ref path) = self.persist_path {
            let new_routes = Self::load(path);
            let mut routes = self.routes.write().await;
            *routes = new_routes;
        }
        if let Some(ref path) = self.room_persist_path {
            let new_rooms = Self::load_rooms(path);
            let mut room_bots = self.room_bots.write().await;
            *room_bots = new_rooms;
        }
        Ok(())
    }

    /// Return all user_id → profile_id mappings.
    pub async fn list_routes(&self) -> Vec<(String, String)> {
        let routes = self.routes.read().await;
        routes
            .iter()
            .map(|(k, v)| (k.clone(), v.profile_id.clone()))
            .collect()
    }

    pub async fn list_entries(&self) -> Vec<(String, BotEntry)> {
        let routes = self.routes.read().await;
        routes.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// Atomically persist routes to disk (write to temp file, then rename).
    /// Serializes under lock, then releases before file I/O.
    fn persist(&self, routes: &HashMap<String, BotEntry>) -> Result<()> {
        let Some(ref path) = self.persist_path else {
            return Ok(());
        };
        let data =
            serde_json::to_string_pretty(routes).wrap_err("failed to serialize bot routes")?;
        let tmp_path = path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &data).wrap_err_with(|| {
            format!(
                "failed to write bot routes temp file '{}'",
                tmp_path.display()
            )
        })?;
        std::fs::rename(&tmp_path, path).wrap_err_with(|| {
            format!(
                "failed to rename bot routes temp file '{}' to '{}'",
                tmp_path.display(),
                path.display()
            )
        })?;
        Ok(())
    }

    /// Persist room-bot mappings to disk.
    fn persist_rooms(&self, room_bots: &HashMap<String, HashSet<String>>) -> Result<()> {
        let Some(ref path) = self.room_persist_path else {
            return Ok(());
        };
        // Serialize HashSet as Vec for JSON compatibility.
        let serializable: HashMap<&String, Vec<&String>> = room_bots
            .iter()
            .map(|(k, v)| (k, v.iter().collect()))
            .collect();
        let data = serde_json::to_string_pretty(&serializable)
            .wrap_err("failed to serialize room-bot map")?;
        let tmp_path = path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &data).wrap_err_with(|| {
            format!(
                "failed to write room-bot map temp file '{}'",
                tmp_path.display()
            )
        })?;
        std::fs::rename(&tmp_path, path).wrap_err_with(|| {
            format!(
                "failed to rename room-bot map temp file '{}' to '{}'",
                tmp_path.display(),
                path.display()
            )
        })?;
        Ok(())
    }

    /// Load room-bot mappings from a JSON file.
    fn load_rooms(path: &std::path::Path) -> HashMap<String, HashSet<String>> {
        match std::fs::read_to_string(path) {
            Ok(data) => {
                let map: HashMap<String, Vec<String>> =
                    serde_json::from_str(&data).unwrap_or_default();
                map.into_iter()
                    .map(|(k, v)| (k, v.into_iter().collect()))
                    .collect()
            }
            Err(_) => HashMap::new(),
        }
    }
}

fn is_matrix_user_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '=' | '-' | '/' | ':' | '@')
}

fn contains_exact_matrix_user_id_mention(text: &str, user_id: &str) -> bool {
    for (idx, _) in text.match_indices(user_id) {
        let start_ok = text[..idx]
            .chars()
            .next_back()
            .is_none_or(|c| !is_matrix_user_id_char(c));
        let end_idx = idx + user_id.len();
        let end_ok = text[end_idx..]
            .chars()
            .next()
            .is_none_or(|c| !is_matrix_user_id_char(c));
        if start_ok && end_ok {
            return true;
        }
    }
    false
}

async fn route_by_explicit_target(bot_router: &BotRouter, content: &Value) -> Option<String> {
    let target_user_id = content
        .get(CONTENT_TARGET_USER_ID)
        .or_else(|| content.get(CONTENT_TARGET_USER_ID_LEGACY))
        .and_then(|v| v.as_str())
        .filter(|value| !value.is_empty())?;
    bot_router.route(target_user_id).await
}

/// Extract the deduplicated `org.octos.broadcast_targets` list from event
/// content. Capable clients (Robrix management rooms) attach the room's
/// bound child-bot user IDs here so `/allbots` knows where to fan out.
fn broadcast_target_matrix_user_ids(content: &Value) -> Vec<String> {
    let Some(targets) = content
        .get(CONTENT_BROADCAST_TARGETS)
        .and_then(|value| value.as_array())
    else {
        return Vec::new();
    };

    let mut deduped = Vec::new();
    for target in targets.iter().filter_map(|value| value.as_str()) {
        if !deduped.iter().any(|existing| existing == target) {
            deduped.push(target.to_string());
        }
    }
    deduped
}

async fn route_by_matrix_mention(
    bot_router: &BotRouter,
    content: &Value,
    body_text: &str,
) -> Option<String> {
    if let Some(user_ids) = content
        .get("m.mentions")
        .and_then(|v| v.get("user_ids"))
        .and_then(|v| v.as_array())
    {
        for user_id in user_ids.iter().filter_map(|v| v.as_str()) {
            if let Some(profile_id) = bot_router.route(user_id).await {
                return Some(profile_id);
            }
        }
    }

    if let Some(profile_id) = bot_router.route_by_mention(body_text).await {
        return Some(profile_id);
    }

    let formatted_body = content
        .get("formatted_body")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !formatted_body.is_empty() {
        return bot_router.route_by_mention(formatted_body).await;
    }

    None
}

/// Shared state for the appservice HTTP handlers.
#[derive(Clone)]
struct AppserviceState {
    inbound_tx: mpsc::Sender<InboundMessage>,
    homeserver: String,
    as_token: String,
    hs_token: String,
    bot_user_id: String,
    server_name: String,
    user_prefix: String,
    http: reqwest::Client,
    registered_users: Arc<RwLock<HashSet<String>>>,
    dedup: Arc<MessageDedup>,
    bot_router: Arc<BotRouter>,
    bot_manager: Option<Arc<dyn BotManager>>,
    /// Directory for downloaded media files (inbound images, files, audio, video).
    media_dir: PathBuf,
}

fn error_json_response(
    status: StatusCode,
    message: impl std::fmt::Display,
) -> (StatusCode, axum::Json<Value>) {
    (
        status,
        axum::Json(json!({
            "error": message.to_string(),
        })),
    )
}

/// Query parameters for Matrix Appservice endpoints.
#[derive(Deserialize)]
struct AccessTokenQuery {
    access_token: Option<String>,
}

/// Matrix Appservice channel.
///
/// Receives events from the homeserver via the Application Service API and sends
/// messages using the Client-Server API with `?user_id=` identity assertion.
pub struct MatrixChannel {
    homeserver: String,
    as_token: String,
    hs_token: String,
    server_name: String,
    sender_localpart: String,
    user_prefix: String,
    bot_user_id: String,
    port: u16,
    shutdown: Arc<AtomicBool>,
    http: reqwest::Client,
    registered_users: Arc<RwLock<HashSet<String>>>,
    dedup: Arc<MessageDedup>,
    bot_router: Arc<BotRouter>,
    bot_manager: std::sync::OnceLock<Arc<dyn BotManager>>,
    /// Operator override users for break-glass bot management.
    admin_allowed_senders: HashSet<String>,
    /// Bounded FIFO of event_id → sender_user_id so edit_message can reuse the correct identity
    /// without growing unbounded over a long-lived gateway process.
    event_senders: Arc<RwLock<VecDeque<(String, String)>>>,
    /// M7.3 swarm supervisor state. `None` means the supervisor contract is
    /// disabled and the channel behaves exactly like pre-M7.3 (invariant 5).
    swarm_supervisor: Option<Arc<SwarmSupervisorState>>,
    /// Directory for downloaded media files (inbound images, files, audio, video).
    media_dir: PathBuf,
}

impl MatrixChannel {
    /// Create a new Matrix Appservice channel.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        homeserver: &str,
        as_token: &str,
        hs_token: &str,
        server_name: &str,
        sender_localpart: &str,
        user_prefix: &str,
        port: u16,
        shutdown: Arc<AtomicBool>,
    ) -> Self {
        let bot_user_id = format!("@{sender_localpart}:{server_name}");
        Self {
            homeserver: homeserver.trim_end_matches('/').to_string(),
            as_token: as_token.to_string(),
            hs_token: hs_token.to_string(),
            server_name: server_name.to_string(),
            sender_localpart: sender_localpart.to_string(),
            user_prefix: user_prefix.to_string(),
            bot_user_id,
            port,
            shutdown,
            http: reqwest::Client::new(),
            registered_users: Arc::new(RwLock::new(HashSet::new())),
            dedup: Arc::new(MessageDedup::new()),
            bot_router: Arc::new(BotRouter::new(None)),
            bot_manager: std::sync::OnceLock::new(),
            admin_allowed_senders: HashSet::new(),
            event_senders: Arc::new(RwLock::new(VecDeque::new())),
            swarm_supervisor: None,
            media_dir: std::env::temp_dir().join("octos-matrix-media"),
        }
    }

    /// Set the directory for downloaded media files.
    pub fn with_media_dir(mut self, media_dir: PathBuf) -> Self {
        self.media_dir = media_dir;
        self
    }

    /// Restrict bot-management slash commands to the given Matrix user IDs.
    pub fn with_admin_allowed_senders(mut self, allowed_senders: Vec<String>) -> Self {
        self.admin_allowed_senders = allowed_senders.into_iter().collect();
        self
    }

    pub fn is_operator_sender(&self, sender: &str) -> bool {
        self.admin_allowed_senders.contains(sender)
    }

    /// Configure a `BotRouter` with persistence at `{data_dir}/matrix-bot-routes.json`.
    pub fn with_bot_router(mut self, data_dir: &std::path::Path) -> Self {
        let path = data_dir.join("matrix-bot-routes.json");
        self.bot_router = Arc::new(BotRouter::new(Some(path)));
        self
    }

    /// Attach a `BotManager` for handling slash commands (`/createbot`, `/deletebot`, `/listbots`).
    ///
    /// Can be called after construction (before `start()`) since the channel is
    /// typically wrapped in `Arc` by the time the gateway wires bot management.
    pub fn set_bot_manager(&self, mgr: Arc<dyn BotManager>) {
        let _ = self.bot_manager.set(mgr);
    }

    /// Returns a reference to the bot router.
    pub fn bot_router(&self) -> &Arc<BotRouter> {
        &self.bot_router
    }

    /// Register a bot mapping and provision the Matrix virtual user on the homeserver.
    pub async fn register_bot(&self, matrix_user_id: &str, profile_id: &str) -> Result<()> {
        self.register_bot_owned(matrix_user_id, profile_id, "", BotVisibility::Public)
            .await
    }

    pub async fn register_bot_owned(
        &self,
        matrix_user_id: &str,
        profile_id: &str,
        owner: &str,
        visibility: BotVisibility,
    ) -> Result<()> {
        let localpart = managed_localpart(matrix_user_id, &self.server_name).ok_or_else(|| {
            eyre::eyre!("invalid Matrix user ID for this homeserver: {matrix_user_id}")
        })?;
        self.register_user(localpart).await?;
        self.bot_router
            .register_entry(matrix_user_id, profile_id, owner, visibility)
            .await?;
        self.registered_users
            .write()
            .await
            .insert(matrix_user_id.to_string());
        Ok(())
    }

    /// Remove a bot mapping from the router, leave joined rooms, and clean up room mappings.
    pub async fn unregister_bot(&self, matrix_user_id: &str) -> Result<()> {
        // Look up profile_id before removing the user route
        if let Some(profile_id) = self.bot_router.route(matrix_user_id).await {
            // Leave all rooms the bot is in (best-effort, non-fatal)
            let rooms = self.bot_router.rooms_for_profile(&profile_id).await;
            for room_id in &rooms {
                leave_room_via_appservice(
                    &self.http,
                    &self.homeserver,
                    &self.as_token,
                    room_id,
                    matrix_user_id,
                )
                .await?;
            }
            self.bot_router.remove_bot_from_rooms(&profile_id).await?;
        }
        self.bot_router.unregister(matrix_user_id).await?;
        self.registered_users.write().await.remove(matrix_user_id);
        Ok(())
    }

    /// Returns the fully-qualified Matrix user ID for the bot.
    pub fn bot_user_id(&self) -> &str {
        &self.bot_user_id
    }

    /// Build a full URL for a homeserver API path.
    fn make_api_url(&self, path: &str) -> String {
        format!("{}{}", self.homeserver, path)
    }

    /// Register a virtual user with the homeserver using the appservice token.
    ///
    /// This calls `POST /_matrix/client/v3/register` with `type: m.login.application_service`.
    /// If the user is already registered (M_USER_IN_USE), we treat it as success.
    async fn register_user(&self, localpart: &str) -> Result<()> {
        register_user_via_appservice(&self.http, &self.homeserver, &self.as_token, localpart).await
    }

    /// Generate a Matrix Appservice registration YAML file at `{data_dir}/matrix-appservice-registration.yaml`.
    /// Returns the file path. Does NOT overwrite existing files.
    pub fn generate_registration(&self, data_dir: &std::path::Path) -> Result<PathBuf> {
        use std::io::Write;
        let path = data_dir.join("matrix-appservice-registration.yaml");

        #[derive(Serialize)]
        struct RegistrationNamespace {
            exclusive: bool,
            regex: String,
        }

        #[derive(Serialize)]
        struct RegistrationNamespaces {
            users: Vec<RegistrationNamespace>,
            aliases: Vec<RegistrationNamespace>,
            rooms: Vec<RegistrationNamespace>,
        }

        #[derive(Serialize)]
        struct RegistrationYaml {
            id: String,
            url: String,
            as_token: String,
            hs_token: String,
            sender_localpart: String,
            rate_limited: bool,
            namespaces: RegistrationNamespaces,
        }

        let registration = RegistrationYaml {
            id: "octos-matrix-appservice".to_string(),
            url: format!("http://localhost:{}", self.port),
            as_token: self.as_token.clone(),
            hs_token: self.hs_token.clone(),
            sender_localpart: self.sender_localpart.clone(),
            rate_limited: false,
            namespaces: RegistrationNamespaces {
                users: vec![RegistrationNamespace {
                    exclusive: true,
                    regex: format!("@{}.*:{}", self.user_prefix, self.server_name),
                }],
                aliases: vec![],
                rooms: vec![],
            },
        };
        let yaml = serde_yml::to_string(&registration)
            .wrap_err("failed to serialize registration YAML")?;

        // Atomic: create_new(true) fails if file already exists (no TOCTOU race)
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                f.write_all(yaml.as_bytes()).wrap_err_with(|| {
                    format!("failed to write registration YAML to {}", path.display())
                })?;
                info!(?path, "generated Matrix appservice registration YAML");
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                info!(
                    ?path,
                    "registration YAML already exists, skipping generation"
                );
            }
            Err(e) => {
                return Err(eyre::eyre!(e).wrap_err(format!(
                    "failed to create registration YAML at {}",
                    path.display()
                )));
            }
        }
        Ok(path)
    }
}

/// Percent-encode a string for use in URL path segments.
///
/// Encodes characters that are not unreserved (per RFC 3986) and also encodes
/// characters commonly found in Matrix identifiers that could conflict with
/// URL parsing (`:`, `@`, `!`, `#`).
pub(crate) fn percent_encode_path(s: &str) -> String {
    let mut encoded = String::with_capacity(s.len() * 3);
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => {
                encoded.push('%');
                encoded.push_str(&format!("{byte:02X}"));
            }
        }
    }
    encoded
}

/// Reject an outbound media file whose on-disk size exceeds `cap`, by reading
/// metadata only (never the whole file). Prevents a runaway producer from
/// OOMing the gateway via `upload_media`'s `std::fs::read`.
fn check_upload_within_cap(file_path: &std::path::Path, cap: u64) -> Result<u64> {
    let file_size = std::fs::metadata(file_path)
        .map(|m| m.len())
        .wrap_err_with(|| format!("failed to stat media file: {}", file_path.display()))?;
    if file_size > cap {
        eyre::bail!(
            "media file exceeds max upload size ({file_size} bytes > {cap} byte cap): {}",
            file_path.display()
        );
    }
    Ok(file_size)
}

/// Guess a MIME content type from a file path's extension for Matrix media
/// uploads. Unknown extensions fall back to `application/octet-stream`
/// (rendered as `m.file` on the receiving side).
fn media_content_type(file_path: &std::path::Path) -> &'static str {
    let ext = file_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "m4a" => "audio/mp4",
        "flac" => "audio/flac",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// Check if a Matrix user ID is managed by this appservice (bot or virtual user).
fn is_managed_user(
    user_id: &str,
    bot_user_id: &str,
    server_suffix: &str,
    user_prefix: &str,
) -> bool {
    if user_id == bot_user_id {
        return true;
    }
    user_id
        .strip_prefix('@')
        .and_then(|s| s.strip_suffix(server_suffix))
        .is_some_and(|lp| lp.starts_with(user_prefix))
}

fn managed_localpart<'a>(user_id: &'a str, server_name: &str) -> Option<&'a str> {
    user_id
        .strip_prefix('@')
        .and_then(|s| s.strip_suffix(&format!(":{server_name}")))
}

fn default_appservice_bind_addr(port: u16) -> String {
    format!("0.0.0.0:{port}")
}

async fn register_user_via_appservice(
    http: &reqwest::Client,
    homeserver: &str,
    as_token: &str,
    localpart: &str,
) -> Result<()> {
    let url = format!("{homeserver}/_matrix/client/v3/register");
    let body = json!({
        "type": "m.login.application_service",
        "username": localpart,
    });

    let resp = http
        .post(&url)
        .bearer_auth(as_token)
        .json(&body)
        .send()
        .await
        .wrap_err("failed to send register request to homeserver")?;

    let status = resp.status();
    if status.is_success() {
        info!(localpart, "registered virtual user with homeserver");
        return Ok(());
    }

    let resp_body: Value = resp
        .json()
        .await
        .unwrap_or_else(|_| json!({"errcode": "UNKNOWN"}));
    let errcode = resp_body
        .get("errcode")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if errcode == "M_USER_IN_USE" {
        debug!(localpart, "virtual user already registered");
        Ok(())
    } else {
        let error_msg = resp_body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        warn!(
            localpart,
            status = status.as_u16(),
            errcode,
            error = error_msg,
            "failed to register virtual user"
        );
        Err(eyre::eyre!(
            "register user {localpart} failed: {status} {errcode} {error_msg}"
        ))
    }
}

async fn join_room_via_appservice(
    http: &reqwest::Client,
    homeserver: &str,
    as_token: &str,
    room_id: &str,
    user_id: &str,
) -> Result<()> {
    let url = format!(
        "{homeserver}/_matrix/client/v3/rooms/{}/join?user_id={}",
        percent_encode_path(room_id),
        percent_encode_path(user_id),
    );
    let resp = http
        .post(&url)
        .bearer_auth(as_token)
        .json(&json!({}))
        .send()
        .await
        .wrap_err("failed to send join request to homeserver")?;

    if resp.status().is_success() {
        return Ok(());
    }

    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    Err(eyre::eyre!(
        "join room failed: room_id={room_id} user_id={user_id} status={status} body={body}"
    ))
}

async fn leave_room_via_appservice(
    http: &reqwest::Client,
    homeserver: &str,
    as_token: &str,
    room_id: &str,
    user_id: &str,
) -> Result<()> {
    let url = format!(
        "{homeserver}/_matrix/client/v3/rooms/{}/leave?user_id={}",
        percent_encode_path(room_id),
        percent_encode_path(user_id),
    );
    let resp = match http
        .post(&url)
        .bearer_auth(as_token)
        .json(&json!({}))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(room_id, user_id, error = %e, "leave room request failed (non-fatal)");
            return Ok(());
        }
    };

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        warn!(room_id, user_id, %status, "leave room failed (non-fatal): {body}");
    }
    Ok(())
}

// ── Axum handlers ────────────────────────────────────────────────────────────

/// Validate the hs_token from either query parameter or Authorization header.
fn validate_hs_token(
    query: &AccessTokenQuery,
    headers: &HeaderMap,
    expected: &str,
) -> std::result::Result<(), StatusCode> {
    let bearer_token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    // If both query and header tokens are present, reject if they disagree.
    if let (Some(qt), Some(ht)) = (query.access_token.as_deref(), bearer_token) {
        if qt != ht {
            return Err(StatusCode::FORBIDDEN);
        }
    }

    // Accept whichever token is present (query takes priority).
    let token = query.access_token.as_deref().or(bearer_token);
    match token {
        Some(t) if bool::from(t.as_bytes().ct_eq(expected.as_bytes())) => Ok(()),
        Some(_) => Err(StatusCode::FORBIDDEN),
        None => Err(StatusCode::UNAUTHORIZED),
    }
}

/// PUT /_matrix/app/v1/transactions/{txn_id}
///
/// Receives events from the homeserver. Validates hs_token, deduplicates by
/// txn_id, extracts m.room.message events, and forwards them as InboundMessages.
async fn handle_transaction(
    State(state): State<AppserviceState>,
    Path(txn_id): Path<String>,
    Query(query): Query<AccessTokenQuery>,
    headers: HeaderMap,
    body: String,
) -> impl IntoResponse {
    // Validate hs_token
    if let Err(status) = validate_hs_token(&query, &headers, &state.hs_token) {
        return (status, "{}").into_response();
    }

    // Parse body
    let payload: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            warn!(txn_id, error = %e, "failed to parse transaction body");
            return (StatusCode::BAD_REQUEST, "{}").into_response();
        }
    };

    let events = match payload.get("events").and_then(|v| v.as_array()) {
        Some(events) => events,
        None => {
            debug!(txn_id, "transaction has no events array");
            return (StatusCode::OK, "{}").into_response();
        }
    };

    // Dedup only after the transaction is structurally valid so a malformed
    // request does not poison later homeserver retries that reuse the txn_id.
    if state.dedup.is_duplicate(&txn_id) {
        debug!(txn_id, "duplicate transaction, skipping");
        return (StatusCode::OK, "{}").into_response();
    }

    let server_suffix = format!(":{}", state.server_name);

    for event in events {
        let event_type = event.get("type").and_then(|v| v.as_str()).unwrap_or("");

        // Handle m.room.member invite events — auto-join rooms we're invited to
        if event_type == EVENT_ROOM_MEMBER {
            if let Some(membership) = event
                .get("content")
                .and_then(|c| c.get("membership"))
                .and_then(|v| v.as_str())
            {
                if membership == MEMBERSHIP_INVITE {
                    let state_key = event
                        .get("state_key")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let is_our_user = is_managed_user(
                        state_key,
                        &state.bot_user_id,
                        &server_suffix,
                        &state.user_prefix,
                    );
                    if is_our_user {
                        let room_id = event.get("room_id").and_then(|v| v.as_str()).unwrap_or("");
                        debug!(
                            txn_id,
                            room_id,
                            invited_user = state_key,
                            "received invite for managed user"
                        );
                        if room_id.is_empty() {
                            warn!(
                                txn_id,
                                invited_user = state_key,
                                "invite event missing room_id"
                            );
                            continue;
                        }
                        let Some(localpart) = managed_localpart(state_key, &state.server_name)
                        else {
                            warn!(
                                txn_id,
                                invited_user = state_key,
                                "failed to derive localpart for invite"
                            );
                            continue;
                        };
                        let inviter = event.get("sender").and_then(|v| v.as_str()).unwrap_or("");
                        if let Some(entry) = state.bot_router.get_entry(state_key).await {
                            if entry.visibility == BotVisibility::Private && inviter != entry.owner
                            {
                                if let Err(e) = join_room_via_appservice(
                                    &state.http,
                                    &state.homeserver,
                                    &state.as_token,
                                    room_id,
                                    state_key,
                                )
                                .await
                                {
                                    warn!(txn_id, room_id, invited_user = state_key, error = %e, "failed to join room for private bot rejection");
                                } else {
                                    if let Err(e) = send_text_to_room_as(
                                        &state,
                                        room_id,
                                        "This is a private bot. Only its owner can chat with it.",
                                        state_key,
                                    )
                                    .await
                                    {
                                        warn!(txn_id, room_id, invited_user = state_key, error = %e, "failed to send private bot rejection");
                                    }
                                    let _ = leave_room_via_appservice(
                                        &state.http,
                                        &state.homeserver,
                                        &state.as_token,
                                        room_id,
                                        state_key,
                                    )
                                    .await;
                                }
                                continue;
                            }
                        }
                        match register_user_via_appservice(
                            &state.http,
                            &state.homeserver,
                            &state.as_token,
                            localpart,
                        )
                        .await
                        {
                            Ok(()) => {
                                state
                                    .registered_users
                                    .write()
                                    .await
                                    .insert(state_key.to_string());
                            }
                            Err(e) => {
                                warn!(txn_id, invited_user = state_key, error = %e, "failed to register invited managed user");
                                continue;
                            }
                        }
                        if let Err(e) = join_room_via_appservice(
                            &state.http,
                            &state.homeserver,
                            &state.as_token,
                            room_id,
                            state_key,
                        )
                        .await
                        {
                            warn!(txn_id, room_id, invited_user = state_key, error = %e, "failed to join invited room");
                        } else {
                            // Record room → bot mapping for routing.
                            // When only one bot is in a room, messages route to
                            // that bot without requiring @mention (DM and
                            // single-bot group rooms). When multiple bots are in
                            // the same room, @mention is required to disambiguate.
                            if let Some(profile_id) = state.bot_router.route(state_key).await {
                                if let Err(e) =
                                    state.bot_router.add_room_bot(room_id, &profile_id).await
                                {
                                    warn!(txn_id, room_id, error = %e, "failed to record room-bot mapping");
                                }
                            }
                        }
                    }
                }
            }
        }

        // Only process m.room.message events
        if event_type != EVENT_ROOM_MESSAGE {
            continue;
        }

        let sender = event.get("sender").and_then(|v| v.as_str()).unwrap_or("");

        // Ignore messages from our own bot or virtual users
        if is_managed_user(
            sender,
            &state.bot_user_id,
            &server_suffix,
            &state.user_prefix,
        ) {
            continue;
        }

        let room_id = event.get("room_id").and_then(|v| v.as_str()).unwrap_or("");
        let content = match event.get("content") {
            Some(c) => c,
            None => continue,
        };

        let msgtype = content
            .get("msgtype")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        // Accept text and media message types; skip everything else
        // (e.g. m.location, m.notice from other bots).
        let is_media = matches!(
            msgtype,
            MSGTYPE_IMAGE | MSGTYPE_FILE | MSGTYPE_AUDIO | MSGTYPE_VIDEO
        );
        if msgtype != MSGTYPE_TEXT && !is_media {
            continue;
        }

        let body_text = content.get("body").and_then(|v| v.as_str()).unwrap_or("");

        // For media messages, download the file from the mxc:// URL so the
        // agent can use it for vision/tool input. Download failure degrades
        // to a text-only inbound message.
        let mut media = vec![];
        if is_media {
            if let Some(mxc_url) = content.get("url").and_then(|v| v.as_str()) {
                // Use the filename field if available, fall back to body
                // (which is the filename per the Matrix spec).
                let filename = content
                    .get("filename")
                    .and_then(|v| v.as_str())
                    .or(Some(body_text))
                    .filter(|s| !s.is_empty())
                    .unwrap_or("file");

                let download_url = format!(
                    "{}/_matrix/media/v3/download/{}",
                    state.homeserver,
                    mxc_url.strip_prefix("mxc://").unwrap_or(mxc_url),
                );
                let unique_filename = format!(
                    "matrix_{}_{}",
                    chrono::Utc::now().timestamp_millis(),
                    filename,
                );
                match crate::media::download_media(
                    &state.http,
                    &download_url,
                    &[("Authorization", &format!("Bearer {}", state.as_token))],
                    &state.media_dir,
                    &unique_filename,
                )
                .await
                {
                    Ok(local_path) => {
                        info!(
                            mxc_url,
                            filename,
                            ?local_path,
                            "downloaded Matrix media file"
                        );
                        media.push(local_path.to_string_lossy().into_owned());
                    }
                    Err(e) => {
                        warn!(mxc_url, error = %e, "failed to download Matrix media file, continuing without media");
                    }
                }
            }
        }

        // For text messages, body must be non-empty. For media, allow empty body.
        if !is_media && body_text.is_empty() {
            continue;
        }

        let event_id = event
            .get("event_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Route to bot profile: explicit target first, then @mention, then DM room mapping
        let mut metadata = json!({});
        if let Some(action_response) = content.get(CONTENT_ACTION_RESPONSE) {
            metadata[CONTENT_ACTION_RESPONSE] = action_response.clone();
        }
        // Phase 4: approval responses (Approve/Deny button presses) flow back
        // to the session actor via InboundMessage metadata.
        if let Some(approval_response) = content.get(CONTENT_APPROVAL_RESPONSE) {
            metadata[CONTENT_APPROVAL_RESPONSE] = approval_response.clone();
        }
        if let Some(profile_id) = route_by_explicit_target(&state.bot_router, content).await {
            metadata[METADATA_TARGET_PROFILE_ID] = json!(profile_id);
        } else if let Some(profile_id) =
            route_by_matrix_mention(&state.bot_router, content, body_text).await
        {
            metadata[METADATA_TARGET_PROFILE_ID] = json!(profile_id);
        } else if let Some(profile_id) = state.bot_router.route_by_room(room_id).await {
            metadata[METADATA_TARGET_PROFILE_ID] = json!(profile_id);
        }

        if let Some(profile_id) = metadata
            .get(METADATA_TARGET_PROFILE_ID)
            .and_then(|value| value.as_str())
        {
            if let Some(matrix_user_id) = state.bot_router.reverse_route(profile_id).await {
                metadata[METADATA_TARGET_MATRIX_USER_ID] = json!(matrix_user_id);
            }
        }

        if let Some(target_user_id) = metadata
            .get(METADATA_TARGET_MATRIX_USER_ID)
            .and_then(|value| value.as_str())
        {
            if let Some(entry) = state.bot_router.get_entry(target_user_id).await {
                if entry.visibility == BotVisibility::Private && sender != entry.owner {
                    if let Err(e) = send_text_to_room_as(
                        &state,
                        room_id,
                        "This is a private bot. Only its owner can chat with it.",
                        target_user_id,
                    )
                    .await
                    {
                        warn!(error = %e, room_id, target_user_id, "failed to send private bot message rejection");
                    }
                    continue;
                }
            }
        }

        // Intercept slash commands before routing to the agent. Runs AFTER
        // routing so commands explicitly aimed at a child bot (mention /
        // explicit target) flow through to that bot instead of being
        // swallowed by BotFather.
        if let Some(response) = handle_slash_command(
            &state,
            sender,
            room_id,
            body_text,
            metadata
                .get(METADATA_TARGET_MATRIX_USER_ID)
                .and_then(|value| value.as_str()),
            content,
            event_id.as_deref(),
        )
        .await
        {
            // `/allbots` answers with an empty string on success (the fanned
            // out child-bot replies are the visible outcome) — skip the echo.
            if !response.trim().is_empty()
                && let Err(e) = send_text_to_room(&state, room_id, &response).await
            {
                warn!(error = %e, room_id, "failed to send slash command response");
            }
            continue;
        }

        // For media messages with an empty body, provide a descriptive placeholder.
        let content_text = if body_text.is_empty() && !media.is_empty() {
            "[User sent a file]".to_string()
        } else {
            body_text.to_string()
        };

        let inbound = InboundMessage {
            channel: CHANNEL_NAME.into(),
            sender_id: sender.to_string(),
            chat_id: room_id.to_string(),
            content: content_text,
            timestamp: Utc::now(),
            media,
            metadata,
            message_id: event_id,
            origin: octos_core::MessageOrigin::ExternalUser,
        };

        if state.inbound_tx.send(inbound).await.is_err() {
            warn!(
                txn_id,
                "inbound channel closed while processing Matrix transaction; \
                 returning 500 so the homeserver retries"
            );
            // The dedup check above already recorded this txn_id as seen.
            // Forget it so the homeserver's retry of the same transaction is
            // accepted instead of being dropped as a duplicate. Events
            // enqueued before this failure may be delivered again on retry —
            // at-least-once beats acking a dropped message with 200 OK
            // (which the appservice spec treats as "processed, never retry").
            state.dedup.forget(&txn_id);
            return (StatusCode::INTERNAL_SERVER_ERROR, "{}").into_response();
        }
    }

    (StatusCode::OK, "{}").into_response()
}

// ── Slash command handling ───────────────────────────────────────────────────

/// Check if a message is a slash command and handle it.
/// Returns `Some(response_text)` if it was a slash command, `None` otherwise.
#[allow(clippy::too_many_arguments)]
async fn handle_slash_command(
    state: &AppserviceState,
    sender: &str,
    room_id: &str,
    body: &str,
    target_matrix_user_id: Option<&str>,
    content: &Value,
    source_event_id: Option<&str>,
) -> Option<String> {
    let bot_manager = state.bot_manager.as_ref()?;

    // A slash command explicitly aimed at a child bot (mention / explicit
    // target) belongs to that bot's conversation — don't intercept it.
    if target_matrix_user_id.is_some_and(|target| target != state.bot_user_id) {
        return None;
    }

    let trimmed = body.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let command = parts.next().unwrap_or("");
    let args_str = parts.next().unwrap_or("").trim();

    match command {
        "/createbot" => Some(dispatch_createbot(bot_manager.as_ref(), args_str, sender).await),
        "/deletebot" => Some(dispatch_deletebot(bot_manager.as_ref(), args_str, sender).await),
        "/listbots" | "/listbot" => Some(dispatch_listbots(bot_manager.as_ref(), sender).await),
        "/schedule" => {
            Some(dispatch_schedule(bot_manager.as_ref(), args_str, sender, room_id).await)
        }
        "/schedules" => Some(dispatch_schedules(bot_manager.as_ref(), sender, room_id).await),
        "/unschedule" => {
            Some(dispatch_unschedule(bot_manager.as_ref(), args_str, sender, room_id).await)
        }
        "/allbots" => {
            match dispatch_allbots(state, sender, room_id, args_str, content, source_event_id).await
            {
                Ok(()) => Some(String::new()),
                Err(error) => Some(error),
            }
        }
        "/bothelp" => Some(SLASH_HELP.to_string()),
        _ => None,
    }
}

const SLASH_HELP: &str = "\
**Bot management commands:**

• `/createbot <username> <display_name> [--public|--private] [--prompt \"system prompt\"]`
• Missing visibility defaults to `private`
• `/deletebot <matrix_user_id>`
• `/listbots` (public bots + your private bots)
• `/schedule <task>` (natural-language scheduling in this chat)
• `/schedules` (list this chat's schedules)
• `/unschedule <job-id>`
• `/allbots <message>` (management rooms only)
• `/bothelp`

**Tip:** I'm BotFather — you can chat with me directly, or create your own bot with `/createbot` for a dedicated AI assistant.";

/// Fan a BotFather-room command out to the room's bound child bots.
///
/// Targets come from the event's `org.octos.broadcast_targets` content field
/// (attached by capable clients). Stale bindings — targets with no router
/// entry — are skipped with a warning; private bots reject broadcasts from
/// non-owners. Returns `Ok(())` on dispatch (the child-bot replies are the
/// visible outcome) or `Err(user-facing message)` when nothing was sent.
async fn dispatch_allbots(
    state: &AppserviceState,
    sender: &str,
    room_id: &str,
    args_str: &str,
    content: &Value,
    source_event_id: Option<&str>,
) -> std::result::Result<(), String> {
    if args_str.is_empty() {
        return Err("Usage: `/allbots <message>`".to_string());
    }

    let target_matrix_user_ids = broadcast_target_matrix_user_ids(content)
        .into_iter()
        .filter(|user_id| user_id != &state.bot_user_id)
        .collect::<Vec<_>>();

    if target_matrix_user_ids.is_empty() {
        return Err("No bound child bots were found for this room.".to_string());
    }

    if target_matrix_user_ids.len() > MAX_ALLBOTS_TARGETS {
        return Err(format!(
            "/allbots can target at most {MAX_ALLBOTS_TARGETS} bound child bots at once."
        ));
    }

    let mut deliveries = Vec::new();
    let mut unresolved_targets = Vec::new();
    let mut unbound_targets = Vec::new();
    for target_matrix_user_id in target_matrix_user_ids {
        let Some(entry) = state.bot_router.get_entry(&target_matrix_user_id).await else {
            unresolved_targets.push(target_matrix_user_id);
            continue;
        };

        // Server-side authority: the client-supplied broadcast_targets are
        // untrusted. Only deliver to bots actually bound to THIS room — a
        // forged event must not be able to fan out to a public bot in some
        // other room, or to the sender's own private bot bound elsewhere.
        if !state
            .bot_router
            .is_profile_in_room(room_id, &entry.profile_id)
            .await
        {
            unbound_targets.push(target_matrix_user_id);
            continue;
        }

        if entry.visibility == BotVisibility::Private && sender != entry.owner {
            return Err(format!(
                "You do not have permission to broadcast to private bot `{target_matrix_user_id}`."
            ));
        }

        deliveries.push((target_matrix_user_id, entry.profile_id));
    }

    if deliveries.is_empty() {
        if !unbound_targets.is_empty() {
            return Err(format!(
                "None of the requested bots are bound to this room: {}",
                unbound_targets.join(", ")
            ));
        }
        if !unresolved_targets.is_empty() {
            return Err(format!(
                "Could not resolve any bound child bots for /allbots. Stale bindings: {}",
                unresolved_targets.join(", ")
            ));
        }
        return Err("No bound child bots were found for this room.".to_string());
    }

    let request_id = source_event_id.unwrap_or("allbots");
    info!(
        requester = sender,
        room_id,
        request_id,
        targets = ?deliveries.iter().map(|(target, _)| target).collect::<Vec<_>>(),
        "dispatching /allbots broadcast"
    );
    if !unresolved_targets.is_empty() {
        warn!(
            requester = sender,
            room_id,
            request_id,
            stale_targets = ?unresolved_targets,
            "skipping unresolved stale /allbots bindings"
        );
    }
    if !unbound_targets.is_empty() {
        warn!(
            requester = sender,
            room_id,
            request_id,
            unbound_targets = ?unbound_targets,
            "rejecting /allbots targets not bound to this room"
        );
    }

    for (target_matrix_user_id, profile_id) in deliveries {
        let inbound = InboundMessage {
            channel: CHANNEL_NAME.into(),
            sender_id: sender.to_string(),
            chat_id: room_id.to_string(),
            content: args_str.to_string(),
            timestamp: Utc::now(),
            media: vec![],
            metadata: json!({
                METADATA_TARGET_PROFILE_ID: profile_id,
                METADATA_TARGET_MATRIX_USER_ID: target_matrix_user_id,
                "org.octos.broadcast_request_id": request_id,
                "org.octos.broadcast_origin_room_id": room_id,
                "org.octos.broadcast_source_event_id": source_event_id,
            }),
            message_id: source_event_id.map(str::to_string),
            origin: octos_core::MessageOrigin::ExternalUser,
        };

        state.inbound_tx.send(inbound).await.map_err(|_| {
            "broadcast dispatch failed because the inbound channel is closed".to_string()
        })?;
    }

    Ok(())
}

async fn dispatch_schedule(
    bot_manager: &dyn BotManager,
    request: &str,
    sender: &str,
    room_id: &str,
) -> String {
    if request.trim().is_empty() {
        return "Usage: `/schedule <natural-language task>`".to_string();
    }
    bot_manager
        .schedule_bot_task(request.trim(), sender, room_id)
        .await
        .unwrap_or_else(|e| format!("Failed to create schedule: {e}"))
}

async fn dispatch_schedules(bot_manager: &dyn BotManager, sender: &str, room_id: &str) -> String {
    bot_manager
        .list_schedules(sender, room_id)
        .await
        .unwrap_or_else(|e| format!("Failed to list schedules: {e}"))
}

async fn dispatch_unschedule(
    bot_manager: &dyn BotManager,
    job_id: &str,
    sender: &str,
    room_id: &str,
) -> String {
    if job_id.trim().is_empty() {
        return "Usage: `/unschedule <job-id>`".to_string();
    }
    bot_manager
        .unschedule_bot_task(job_id.trim(), sender, room_id)
        .await
        .unwrap_or_else(|e| format!("Failed to remove schedule: {e}"))
}

async fn dispatch_createbot(mgr: &dyn BotManager, args: &str, sender: &str) -> String {
    if args.is_empty() {
        return "Please provide at least a username.\n\nUsage: `/createbot <username> <display_name> [--public|--private] [--prompt \"system prompt\"]`\n\nExample: `/createbot weather Weather Bot --public --prompt \"You are a weather assistant\"`"
            .to_string();
    }

    let (args, visibility) = extract_visibility_flag(args);
    let (main_args, system_prompt) = extract_prompt_flag(&args);
    let mut tokens = main_args.split_whitespace();
    let Some(username) = tokens.next() else {
        return "Please provide a username.".to_string();
    };
    let name: String = tokens.collect::<Vec<_>>().join(" ");
    let name = if name.is_empty() {
        username.to_string()
    } else {
        name
    };

    match mgr
        .create_bot(
            username,
            &name,
            system_prompt.as_deref(),
            sender,
            visibility.unwrap_or(BotVisibility::Private),
        )
        .await
    {
        Ok(msg) => msg,
        Err(e) => format!("Could not create bot: {e}"),
    }
}

async fn dispatch_deletebot(mgr: &dyn BotManager, args: &str, sender: &str) -> String {
    if args.is_empty() {
        return "Please provide the Matrix user ID to delete.\n\n\
                Usage: `/deletebot <matrix_user_id>`\n\n\
                Example: `/deletebot @bot_weather:localhost`"
            .to_string();
    }
    let matrix_user_id = args.split_whitespace().next().unwrap_or(args);
    match mgr.delete_bot(matrix_user_id, sender).await {
        Ok(msg) => msg,
        Err(e) => format!("Could not delete bot: {e}"),
    }
}

async fn dispatch_listbots(mgr: &dyn BotManager, sender: &str) -> String {
    match mgr.list_bots(sender).await {
        Ok(msg) => msg,
        Err(e) => format!("Could not list bots: {e}"),
    }
}

fn extract_visibility_flag(args: &str) -> (String, Option<BotVisibility>) {
    for (flag, visibility) in [
        ("--public", BotVisibility::Public),
        ("--private", BotVisibility::Private),
    ] {
        if let Some(idx) = args.find(flag) {
            let before = args[..idx].trim();
            let after = args[idx + flag.len()..].trim();
            return match (before.is_empty(), after.is_empty()) {
                (true, true) => (String::new(), Some(visibility)),
                (true, false) => (after.to_string(), Some(visibility)),
                (false, true) => (before.to_string(), Some(visibility)),
                (false, false) => (format!("{before} {after}"), Some(visibility)),
            };
        }
    }
    (args.trim().to_string(), None)
}

/// Extract `--prompt "..."` from the argument string.
/// Returns (remaining_args, optional_prompt).
fn extract_prompt_flag(args: &str) -> (String, Option<String>) {
    let prompt_marker = "--prompt";
    let Some(idx) = args.find(prompt_marker) else {
        return (args.to_string(), None);
    };

    let before = args[..idx].trim().to_string();
    let after = args[idx + prompt_marker.len()..].trim();

    let prompt = if let Some(stripped) = after.strip_prefix('"') {
        // Find closing quote
        if let Some(end) = stripped.find('"') {
            Some(stripped[..end].to_string())
        } else {
            // No closing quote — take everything after the opening quote
            Some(stripped.to_string())
        }
    } else {
        // No quotes — take everything as prompt
        if after.is_empty() {
            None
        } else {
            Some(after.to_string())
        }
    };

    (before, prompt)
}

/// Send a text message to a Matrix room using the appservice bot identity.
async fn send_text_to_room(state: &AppserviceState, room_id: &str, text: &str) -> Result<()> {
    send_text_to_room_as(state, room_id, text, &state.bot_user_id).await
}

async fn send_text_to_room_as(
    state: &AppserviceState,
    room_id: &str,
    text: &str,
    user_id: &str,
) -> Result<()> {
    let txn_id = uuid::Uuid::now_v7().to_string();
    let path = format!(
        "/_matrix/client/v3/rooms/{}/send/m.room.message/{}?user_id={}",
        percent_encode_path(room_id),
        percent_encode_path(&txn_id),
        percent_encode_path(user_id),
    );
    let url = format!("{}{}", state.homeserver, path);
    let formatted_body = markdown_to_matrix_html(text);
    let body = json!({
        "msgtype": MSGTYPE_TEXT,
        "body": text,
        "format": HTML_FORMAT,
        "formatted_body": formatted_body,
    });

    let resp = state
        .http
        .put(&url)
        .bearer_auth(&state.as_token)
        .json(&body)
        .send()
        .await
        .wrap_err("failed to send slash command response")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let err_body = resp.text().await.unwrap_or_default();
        warn!(status = %status, body = %err_body, "Matrix send failed");
    }
    Ok(())
}

// ── User / Room query handlers ──────────────────────────────────────────────

/// GET /_matrix/app/v1/users/{user_id}
///
/// Homeserver queries whether a user belongs to this appservice.
async fn handle_user_query(
    State(state): State<AppserviceState>,
    Path(user_id): Path<String>,
    Query(query): Query<AccessTokenQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(status) = validate_hs_token(&query, &headers, &state.hs_token) {
        return status.into_response();
    }

    if state.registered_users.read().await.contains(&user_id) {
        (StatusCode::OK, "{}").into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

/// GET /_matrix/app/v1/rooms/{room_alias}
///
/// This appservice does not provision room aliases, but exposing the endpoint
/// keeps the appservice surface complete and ensures token validation happens
/// before the homeserver sees a plain router-level 404.
async fn handle_room_query(
    State(state): State<AppserviceState>,
    Path(_room_alias): Path<String>,
    Query(query): Query<AccessTokenQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(status) = validate_hs_token(&query, &headers, &state.hs_token) {
        return status.into_response();
    }
    StatusCode::NOT_FOUND.into_response()
}

/// POST /_matrix/app/v1/ping
///
/// Homeserver health-check ping.
async fn handle_ping(
    State(state): State<AppserviceState>,
    Query(query): Query<AccessTokenQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(status) = validate_hs_token(&query, &headers, &state.hs_token) {
        return status.into_response();
    }
    (StatusCode::OK, "{}").into_response()
}

/// Reload bot routes and registered users from disk.
/// Called by CLI after `create-matrix-bot` or `delete-matrix-bot`.
/// Requires `hs_token` authentication (query param or Bearer header).
async fn handle_reload_bots(
    Query(query): Query<AccessTokenQuery>,
    headers: HeaderMap,
    State(state): State<AppserviceState>,
) -> impl IntoResponse {
    if validate_hs_token(&query, &headers, &state.hs_token).is_err() {
        return error_json_response(StatusCode::FORBIDDEN, "invalid or missing token")
            .into_response();
    }

    if let Err(e) = state.bot_router.reload().await {
        warn!(error = %e, "failed to reload bot routes");
        return error_json_response(StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    // Sync registered_users from the reloaded routes
    let routes = state.bot_router.list_routes().await;
    let mut users = state.registered_users.write().await;
    users.clear();
    users.insert(state.bot_user_id.clone());
    for (matrix_id, _) in &routes {
        users.insert(matrix_id.clone());
    }
    info!(bot_count = routes.len(), "bot routes reloaded");
    (StatusCode::OK, axum::Json(json!({ "reloaded": true }))).into_response()
}

// ── Channel trait implementation ─────────────────────────────────────────────

#[async_trait]
impl Channel for MatrixChannel {
    fn name(&self) -> &str {
        CHANNEL_NAME
    }

    fn max_message_length(&self) -> usize {
        65535
    }

    fn supports_edit(&self) -> bool {
        true
    }

    async fn start(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        info!(
            port = self.port,
            bot = %self.bot_user_id,
            "Starting Matrix appservice channel"
        );

        // Register the bot user with the homeserver
        if let Err(e) = self.register_user(&self.sender_localpart).await {
            warn!(error = %e, "failed to register bot user (may already exist)");
        }

        // Add bot user + all persisted bot routes to registered users set
        {
            let mut users = self.registered_users.write().await;
            users.insert(self.bot_user_id.clone());
            for (matrix_user_id, _profile_id) in self.bot_router.list_routes().await {
                users.insert(matrix_user_id);
            }
        }

        let state = AppserviceState {
            inbound_tx,
            homeserver: self.homeserver.clone(),
            as_token: self.as_token.clone(),
            hs_token: self.hs_token.clone(),
            bot_user_id: self.bot_user_id.clone(),
            server_name: self.server_name.clone(),
            user_prefix: self.user_prefix.clone(),
            http: self.http.clone(),
            registered_users: self.registered_users.clone(),
            dedup: self.dedup.clone(),
            bot_router: self.bot_router.clone(),
            bot_manager: self.bot_manager.get().cloned(),
            media_dir: self.media_dir.clone(),
        };

        let app = Router::new()
            .route(
                "/_matrix/app/v1/transactions/{txn_id}",
                put(handle_transaction),
            )
            .route("/_matrix/app/v1/users/{user_id}", get(handle_user_query))
            .route("/_matrix/app/v1/rooms/{room_alias}", get(handle_room_query))
            .route("/_matrix/app/v1/ping", axum::routing::post(handle_ping))
            .route(
                "/_octos/reload-bots",
                axum::routing::post(handle_reload_bots),
            )
            .with_state(state);

        let addr = default_appservice_bind_addr(self.port);
        info!(port = self.port, "Matrix appservice listening on {addr}");
        let listener = tokio::net::TcpListener::bind(&addr).await?;

        let shutdown = self.shutdown.clone();
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                while !shutdown.load(Ordering::Acquire) {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            })
            .await?;

        info!("Matrix appservice channel stopped");
        Ok(())
    }

    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        self.send_with_id(msg).await?;
        Ok(())
    }

    async fn send_with_id(&self, msg: &OutboundMessage) -> Result<Option<String>> {
        let sender_user_id = msg
            .metadata
            .get(METADATA_SENDER_USER_ID)
            .and_then(|v| v.as_str());

        if let Some(uid) = sender_user_id {
            let registered = self.registered_users.read().await;
            if !registered.contains(uid) {
                return Err(eyre::eyre!(
                    "sender_user_id {uid} is not registered as a managed user"
                ));
            }
        }

        // Handle media files (images, documents, audio, video): upload each
        // to the Matrix media repo, then send one m.* media event per file.
        if !msg.media.is_empty() {
            let caption = if msg.content.is_empty() {
                None
            } else {
                Some(msg.content.as_str())
            };
            let mut last_event_id = None;

            for (i, path_str) in msg.media.iter().enumerate() {
                let file_path = std::path::Path::new(path_str);
                let file_size = std::fs::metadata(file_path).map(|m| m.len()).unwrap_or(0);
                let filename = file_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("file");
                let content_type = media_content_type(file_path);

                info!(
                    path = path_str,
                    size = file_size,
                    content_type,
                    "sending media file via Matrix"
                );

                let mxc_url = self
                    .upload_media(file_path, content_type, sender_user_id)
                    .await?;

                // Only the first file gets the caption.
                let cap = if i == 0 { caption } else { None };
                let event_id = self
                    .send_media_message(
                        &msg.chat_id,
                        &mxc_url,
                        filename,
                        content_type,
                        file_size,
                        cap,
                        sender_user_id,
                    )
                    .await?;
                last_event_id = Some(event_id);
            }

            // Remember which sender sent this event so edit_message can use
            // the same identity.
            if let (Some(uid), Some(event_id)) = (sender_user_id, &last_event_id) {
                let mut event_senders = self.event_senders.write().await;
                if let Some(pos) = event_senders.iter().position(|(id, _)| id == event_id) {
                    event_senders.remove(pos);
                }
                event_senders.push_back((event_id.clone(), uid.to_string()));
                while event_senders.len() > MAX_EVENT_SENDER_CACHE {
                    event_senders.pop_front();
                }
            }

            return Ok(last_event_id);
        }

        let live = msg
            .metadata
            .get("streaming")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let event_id = self
            .send_matrix_message(
                &msg.chat_id,
                &msg.content,
                sender_user_id,
                live,
                &msg.metadata,
            )
            .await?;

        // Remember which sender sent this event so edit_message can use the same identity.
        if let Some(uid) = sender_user_id {
            let mut event_senders = self.event_senders.write().await;
            if let Some(pos) = event_senders.iter().position(|(id, _)| id == &event_id) {
                event_senders.remove(pos);
            }
            event_senders.push_back((event_id.clone(), uid.to_string()));
            while event_senders.len() > MAX_EVENT_SENDER_CACHE {
                event_senders.pop_front();
            }
        }

        Ok(Some(event_id))
    }

    async fn edit_message(&self, chat_id: &str, message_id: &str, new_content: &str) -> Result<()> {
        self.send_replace_event(chat_id, message_id, new_content, true)
            .await
    }

    async fn finish_stream(
        &self,
        chat_id: &str,
        message_id: &str,
        final_content: &str,
    ) -> Result<()> {
        self.send_replace_event(chat_id, message_id, final_content, false)
            .await
    }

    async fn send_typing(&self, chat_id: &str) -> Result<()> {
        self.send_typing_as(chat_id, None).await
    }

    async fn send_typing_as(&self, chat_id: &str, sender_user_id: Option<&str>) -> Result<()> {
        self.set_typing(chat_id, sender_user_id, true).await
    }

    async fn stop_typing(&self, chat_id: &str) -> Result<()> {
        self.stop_typing_as(chat_id, None).await
    }

    async fn stop_typing_as(&self, chat_id: &str, sender_user_id: Option<&str>) -> Result<()> {
        self.set_typing(chat_id, sender_user_id, false).await
    }

    async fn stop(&self) -> Result<()> {
        self.shutdown.store(true, Ordering::Release);
        Ok(())
    }

    async fn health_check(&self) -> Result<ChannelHealth> {
        let url = self.make_api_url(&format!(
            "/_matrix/client/v3/account/whoami?user_id={}",
            percent_encode_path(&self.bot_user_id),
        ));
        match self.http.get(&url).bearer_auth(&self.as_token).send().await {
            Ok(resp) if resp.status().is_success() => Ok(ChannelHealth::Healthy),
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                Ok(ChannelHealth::Down(format!("status={status}: {body}")))
            }
            Err(e) => Ok(ChannelHealth::Down(e.to_string())),
        }
    }
}

impl MatrixChannel {
    async fn set_typing(
        &self,
        chat_id: &str,
        sender_user_id: Option<&str>,
        typing: bool,
    ) -> Result<()> {
        let sender = sender_user_id.unwrap_or(&self.bot_user_id);
        let url = self.make_api_url(&format!(
            "/_matrix/client/v3/rooms/{}/typing/{}?user_id={}",
            percent_encode_path(chat_id),
            percent_encode_path(sender),
            percent_encode_path(sender),
        ));

        let body = if typing {
            json!({
                "typing": true,
                "timeout": 30000,
            })
        } else {
            json!({
                "typing": false,
            })
        };

        let resp = self
            .http
            .put(&url)
            .bearer_auth(&self.as_token)
            .json(&body)
            .send()
            .await
            .wrap_err("failed to send typing indicator to Matrix")?;

        if !resp.status().is_success() {
            debug!(
                status = resp.status().as_u16(),
                typing, "typing indicator request returned non-success"
            );
        }

        Ok(())
    }

    /// Send a message to a Matrix room and return the event_id.
    ///
    /// If `sender_user_id` is `Some`, the request uses that user for identity
    /// assertion (`?user_id=`); otherwise the default `bot_user_id` is used.
    async fn send_matrix_message(
        &self,
        room_id: &str,
        content: &str,
        sender_user_id: Option<&str>,
        live: bool,
        metadata: &Value,
    ) -> Result<String> {
        let txn_id = uuid::Uuid::now_v7().to_string();
        let effective_sender_user_id = sender_user_id.unwrap_or(&self.bot_user_id);
        let mut path = format!(
            "/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            percent_encode_path(room_id),
            percent_encode_path(&txn_id),
        );
        path.push_str("?user_id=");
        path.push_str(&percent_encode_path(effective_sender_user_id));
        let url = self.make_api_url(&path);

        let formatted_body = markdown_to_matrix_html(content);
        let mut body = json!({
            "msgtype": MSGTYPE_TEXT,
            "body": content,
            "format": HTML_FORMAT,
            "formatted_body": formatted_body,
        });
        if live {
            body[LIVE_MARKER] = json!({});
        }
        if let Some(app) = metadata.get(CONTENT_APP) {
            body[CONTENT_APP] = app.clone();
        }
        if let Some(actions) = metadata.get(CONTENT_ACTIONS) {
            body[CONTENT_ACTIONS] = actions.clone();
        }
        if let Some(action_response) = metadata.get(CONTENT_ACTION_RESPONSE) {
            body[CONTENT_ACTION_RESPONSE] = action_response.clone();
        }
        // Phase 4 (docs/ROBRIX-PHASE4-APPROVAL-FLOW-ADR.md): human-approval
        // request envelopes ride the same metadata→content projection as app
        // cards; Robrix renders Approve/Deny buttons, other clients show the
        // plain-text fallback body.
        if let Some(approval_request) = metadata.get(CONTENT_APPROVAL_REQUEST) {
            body[CONTENT_APPROVAL_REQUEST] = approval_request.clone();
        }
        if let Some(approval_response) = metadata.get(CONTENT_APPROVAL_RESPONSE) {
            body[CONTENT_APPROVAL_RESPONSE] = approval_response.clone();
        }

        let resp = self
            .http
            .put(&url)
            .bearer_auth(&self.as_token)
            .json(&body)
            .send()
            .await
            .wrap_err("failed to send message to Matrix")?;

        let status = resp.status();
        let resp_body: Value = resp
            .json()
            .await
            .wrap_err("failed to parse Matrix send response")?;

        if !status.is_success() {
            let errcode = resp_body
                .get("errcode")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let error = resp_body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            return Err(eyre::eyre!(
                "Matrix send failed: status={status} errcode={errcode} error={error}"
            ));
        }

        let event_id = resp_body
            .get("event_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        Ok(event_id)
    }

    /// Upload a file to the Matrix media repository and return the `mxc://` URI.
    async fn upload_media(
        &self,
        file_path: &std::path::Path,
        content_type: &str,
        sender_user_id: Option<&str>,
    ) -> Result<String> {
        // Bound outbound uploads by the same cap as inbound downloads: check
        // the file size via metadata before reading the whole file into memory,
        // so a runaway producer can't OOM the gateway.
        check_upload_within_cap(file_path, crate::media::max_media_bytes())?;

        let data = std::fs::read(file_path)
            .wrap_err_with(|| format!("failed to read media file: {}", file_path.display()))?;

        let filename = file_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file");

        let effective_sender = sender_user_id.unwrap_or(&self.bot_user_id);
        let url = self.make_api_url(&format!(
            "/_matrix/media/v3/upload?filename={}&user_id={}",
            percent_encode_path(filename),
            percent_encode_path(effective_sender),
        ));

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.as_token)
            .header("Content-Type", content_type)
            .body(data)
            .send()
            .await
            .wrap_err("failed to upload media to Matrix")?;

        let status = resp.status();
        let resp_body: Value = resp
            .json()
            .await
            .wrap_err("failed to parse Matrix upload response")?;

        if !status.is_success() {
            let errcode = resp_body
                .get("errcode")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let error = resp_body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            return Err(eyre::eyre!(
                "Matrix media upload failed: status={status} errcode={errcode} error={error}"
            ));
        }

        let content_uri = resp_body
            .get("content_uri")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre::eyre!("Matrix upload response missing content_uri"))?
            .to_string();

        Ok(content_uri)
    }

    /// Send a media message (`m.image`, `m.audio`, `m.video`, or `m.file`) to a Matrix room.
    #[allow(clippy::too_many_arguments)]
    async fn send_media_message(
        &self,
        room_id: &str,
        mxc_url: &str,
        filename: &str,
        content_type: &str,
        file_size: u64,
        caption: Option<&str>,
        sender_user_id: Option<&str>,
    ) -> Result<String> {
        let txn_id = uuid::Uuid::now_v7().to_string();
        let effective_sender = sender_user_id.unwrap_or(&self.bot_user_id);
        let mut path = format!(
            "/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            percent_encode_path(room_id),
            percent_encode_path(&txn_id),
        );
        path.push_str("?user_id=");
        path.push_str(&percent_encode_path(effective_sender));
        let url = self.make_api_url(&path);

        // Select msgtype by MIME type prefix.
        let msgtype = if content_type.starts_with("image/") {
            MSGTYPE_IMAGE
        } else if content_type.starts_with("audio/") {
            MSGTYPE_AUDIO
        } else if content_type.starts_with("video/") {
            MSGTYPE_VIDEO
        } else {
            MSGTYPE_FILE
        };

        let body_text = caption.unwrap_or(filename);

        let body = json!({
            "msgtype": msgtype,
            "body": body_text,
            "url": mxc_url,
            "filename": filename,
            "info": {
                "mimetype": content_type,
                "size": file_size,
            },
        });

        let resp = self
            .http
            .put(&url)
            .bearer_auth(&self.as_token)
            .json(&body)
            .send()
            .await
            .wrap_err("failed to send media message to Matrix")?;

        let status = resp.status();
        let resp_body: Value = resp
            .json()
            .await
            .wrap_err("failed to parse Matrix send response")?;

        if !status.is_success() {
            let errcode = resp_body
                .get("errcode")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let error = resp_body
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            return Err(eyre::eyre!(
                "Matrix media send failed: status={status} errcode={errcode} error={error}"
            ));
        }

        let event_id = resp_body
            .get("event_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        Ok(event_id)
    }

    /// Send `m.replace`. When `live`, includes MSC4357 marker for streaming.
    async fn send_replace_event(
        &self,
        chat_id: &str,
        message_id: &str,
        new_content: &str,
        live: bool,
    ) -> Result<()> {
        let sender = self
            .event_senders
            .read()
            .await
            .iter()
            .rev()
            .find(|(event_id, _)| event_id == message_id)
            .map(|(_, sender)| sender.clone())
            .unwrap_or_else(|| self.bot_user_id.clone());

        let txn_id = uuid::Uuid::now_v7().to_string();
        let url = self.make_api_url(&format!(
            "/_matrix/client/v3/rooms/{}/send/m.room.message/{}?user_id={}",
            percent_encode_path(chat_id),
            percent_encode_path(&txn_id),
            percent_encode_path(&sender),
        ));

        let formatted_body = markdown_to_matrix_html(new_content);
        let mut body = json!({
            "msgtype": MSGTYPE_TEXT,
            "body": format!("* {new_content}"),
            "format": HTML_FORMAT,
            "formatted_body": formatted_body,
            "m.new_content": {
                "msgtype": MSGTYPE_TEXT,
                "body": new_content,
                "format": HTML_FORMAT,
                "formatted_body": formatted_body,
            },
            "m.relates_to": {
                "rel_type": REL_TYPE_REPLACE,
                "event_id": message_id,
            }
        });

        if live {
            body[LIVE_MARKER] = json!({});
            body["m.new_content"][LIVE_MARKER] = json!({});
        }

        let resp = self
            .http
            .put(&url)
            .bearer_auth(&self.as_token)
            .json(&body)
            .send()
            .await
            .wrap_err("failed to send edit event to Matrix")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let resp_body = resp.text().await.unwrap_or_default();
            return Err(eyre::eyre!(
                "Matrix replace event failed: status={status} body={resp_body}"
            ));
        }

        Ok(())
    }
}

// ── M7.3: Matrix-as-supervisor-UI via agent puppets ─────────────────────────
//
// Register sub-agents as Matrix puppet users, route harness events to per-swarm
// rooms, and accept supervisor replies as steering input. The human uses any
// Matrix client (Element on desktop/mobile/web) as the swarm dashboard — octos
// only emits typed events and listens for replies. Zero net-new UI code.
//
// When [`MatrixChannel::swarm_supervisor`] is `None` (no `ensure_swarm_room` or
// `register_subagent_puppet` calls made), all pre-existing Matrix channel
// behavior is unchanged. All pre-existing tests must continue to pass.
//
// **Required permissions:** the bot account MUST hold Matrix admin API
// permissions (synapse: `admin: true`) so it can register puppet users and
// invite them to rooms on behalf of sub-agents. Deployments without admin
// rights must not configure `SwarmSupervisorConfig`. The current
// implementation uses the appservice registration endpoint (already permitted
// for the baseline bot), which works on homeservers that accept appservice
// user creation; on homeservers that require true admin tokens, operators
// must provision an admin-scoped appservice identity.

/// Current schema version for the swarm-supervisor harness event envelope.
///
/// Versioned alongside [`profiles::SWARM_SUPERVISOR_CONFIG_SCHEMA_VERSION`] so
/// clients that replay a Matrix room can detect unsupported shapes.
pub const SWARM_SUPERVISOR_EVENT_SCHEMA_V1: &str = "octos.harness.event.v1";

/// Typed harness event emitted into a swarm supervisor room.
///
/// Mirrors the shape of `octos_agent::HarnessEvent` without depending on the
/// agent crate (octos-bus must stay sibling-free with octos-agent). Invariant 3
/// of the M7.3 contract requires that serialization preserve `kind` and key
/// summary fields — the `#[serde(tag = "kind", rename_all = "snake_case")]`
/// envelope satisfies that exactly.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SwarmHarnessEvent {
    Progress {
        session_id: String,
        task_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workflow: Option<String>,
        phase: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        progress: Option<f64>,
    },
    Phase {
        session_id: String,
        task_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workflow: Option<String>,
        phase: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Artifact {
        session_id: String,
        task_id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    ValidatorResult {
        session_id: String,
        task_id: String,
        validator: String,
        passed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Retry {
        session_id: String,
        task_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Failure {
        session_id: String,
        task_id: String,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retryable: Option<bool>,
    },
}

impl SwarmHarnessEvent {
    /// Stable discriminant — identical to the serialized `"kind"` tag.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Progress { .. } => "progress",
            Self::Phase { .. } => "phase",
            Self::Artifact { .. } => "artifact",
            Self::ValidatorResult { .. } => "validator_result",
            Self::Retry { .. } => "retry",
            Self::Failure { .. } => "failure",
        }
    }

    /// Human-readable one-line summary for the Matrix message `body`.
    ///
    /// The full event JSON is shipped in `formatted_body` via `pre` so any
    /// Matrix client can render the summary inline and expose structured
    /// fields on expand. Consumer code MUST NOT rely on the exact phrasing —
    /// it is tuned for human reading, not machine parsing.
    pub fn summary(&self) -> String {
        match self {
            Self::Progress {
                phase,
                message,
                progress,
                ..
            } => {
                let pct = progress
                    .map(|p| format!(" [{:.0}%]", p.clamp(0.0, 1.0) * 100.0))
                    .unwrap_or_default();
                let msg = message.as_deref().unwrap_or("progress");
                format!("progress {phase}{pct}: {msg}")
            }
            Self::Phase { phase, message, .. } => {
                let msg = message.as_deref().unwrap_or("phase changed");
                format!("phase {phase}: {msg}")
            }
            Self::Artifact { name, path, .. } => match path {
                Some(p) => format!("artifact {name} -> {p}"),
                None => format!("artifact {name}"),
            },
            Self::ValidatorResult {
                validator,
                passed,
                message,
                ..
            } => {
                let status = if *passed { "passed" } else { "failed" };
                let msg = message.as_deref().unwrap_or("");
                if msg.is_empty() {
                    format!("validator {validator} {status}")
                } else {
                    format!("validator {validator} {status}: {msg}")
                }
            }
            Self::Retry {
                attempt, message, ..
            } => {
                let label = attempt.map(|a| format!(" #{a}")).unwrap_or_default();
                let msg = message.as_deref().unwrap_or("retrying");
                format!("retry{label}: {msg}")
            }
            Self::Failure {
                message, retryable, ..
            } => {
                let tag = match retryable {
                    Some(true) => " (retryable)",
                    Some(false) => " (permanent)",
                    None => "",
                };
                format!("failure{tag}: {message}")
            }
        }
    }
}

/// A supervisor reply routed from the swarm room back to the addressed puppet.
///
/// Produced by [`MatrixChannel::handle_supervisor_reply`] when a human reply
/// targets exactly one puppet (either via mention or via in-reply-to). Callers
/// in the gateway layer translate this into a `SteeringMessage::FollowUp` and
/// deliver it to the sub-agent session queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SteeringInput {
    /// The swarm session the reply belongs to.
    pub session_id: String,
    /// The sub-agent label originally registered via
    /// [`MatrixChannel::register_subagent_puppet`].
    pub agent_label: String,
    /// The fully-qualified Matrix puppet user ID (`@swarm_<label>:server`).
    pub puppet_user_id: MatrixUserId,
    /// The supervisor's Matrix user ID (the sender).
    pub supervisor_user_id: String,
    /// The reply message body with the `@puppet` mention already stripped.
    pub body: String,
}

/// A Matrix user ID newtype — `@localpart:server_name`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MatrixUserId(String);

impl MatrixUserId {
    pub fn new(user_id: impl Into<String>) -> Self {
        Self(user_id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MatrixUserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A Matrix room ID newtype — `!opaque:server_name`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MatrixRoomId(String);

impl MatrixRoomId {
    pub fn new(room_id: impl Into<String>) -> Self {
        Self(room_id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MatrixRoomId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Per-swarm supervisor state held by a [`MatrixChannel`].
///
/// Keeps the `session_id → MatrixRoomId` and `(session_id, agent_label) →
/// MatrixUserId` maps idempotent — the first call provisions the resource and
/// caches the identifier; subsequent calls return the cached value without
/// re-hitting the homeserver. This satisfies invariants 1 and 2.
#[derive(Clone, Debug)]
pub(crate) struct SwarmSupervisorState {
    /// Localpart prefix for puppet users (e.g. `"swarm_"`).
    puppet_prefix: String,
    /// Localpart prefix for swarm rooms (e.g. `"swarm_"`).
    room_prefix: String,
    /// Homeserver base URL (no trailing slash).
    homeserver: String,
    /// Appservice token (`AS-Token`).
    as_token: String,
    /// Homeserver name (e.g. `"example.org"`).
    server_name: String,
    /// Matrix user IDs of configured supervisors — invited to every swarm
    /// room at creation time.
    supervisor_user_ids: Vec<String>,
    /// session_id → room ID (idempotent).
    rooms: Arc<RwLock<HashMap<String, MatrixRoomId>>>,
    /// (session_id, agent_label) → puppet user ID (idempotent).
    puppets: Arc<RwLock<HashMap<(String, String), MatrixUserId>>>,
    /// Shared HTTP client.
    http: reqwest::Client,
}

impl SwarmSupervisorState {
    /// Compute the canonical puppet user ID for `(session, label)`.
    ///
    /// Matches [`register_subagent_puppet`](MatrixChannel::register_subagent_puppet)
    /// / [`puppet_user_id_for`](Self::puppet_user_id_for) — `localpart` is
    /// sanitized to Matrix's allowed character set, truncated if needed.
    fn puppet_user_id_for(&self, session_id: &str, agent_label: &str) -> MatrixUserId {
        let localpart = puppet_localpart(&self.puppet_prefix, session_id, agent_label);
        MatrixUserId(format!("@{localpart}:{}", self.server_name))
    }

    /// Compute the canonical room alias localpart for `session_id`.
    fn room_alias_localpart(&self, session_id: &str) -> String {
        room_alias_localpart(&self.room_prefix, session_id)
    }
}

/// Configuration inputs required to enable the swarm supervisor UI.
///
/// Plumbed from [`crate::profiles::SwarmSupervisorConfig`](../../../octos_cli/profiles/struct.SwarmSupervisorConfig.html)
/// in the CLI (octos-cli cannot be imported from octos-bus; callers pass the
/// already-validated fields directly).
#[derive(Clone, Debug)]
pub struct SwarmSupervisorParams {
    pub puppet_prefix: String,
    pub room_prefix: String,
    pub supervisor_user_ids: Vec<String>,
}

impl Default for SwarmSupervisorParams {
    fn default() -> Self {
        Self {
            puppet_prefix: "swarm_".to_string(),
            room_prefix: "swarm_".to_string(),
            supervisor_user_ids: Vec::new(),
        }
    }
}

/// Sanitize a string fragment to the characters Matrix allows in a localpart.
///
/// Matrix localparts accept `a-z 0-9 . _ = - /`. We lowercase ASCII, replace
/// everything else with `_`, and collapse runs of `_` so labels stay readable.
fn sanitize_localpart_fragment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_underscore = false;
    for ch in raw.chars() {
        let mapped = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else if matches!(ch, '.' | '_' | '=' | '-' | '/') {
            ch
        } else {
            '_'
        };
        if mapped == '_' {
            if prev_underscore {
                continue;
            }
            prev_underscore = true;
        } else {
            prev_underscore = false;
        }
        out.push(mapped);
    }
    // Trim trailing underscores for visual cleanliness.
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        out.push('x');
    }
    out
}

fn puppet_localpart(prefix: &str, session_id: &str, agent_label: &str) -> String {
    // 255 is the Matrix localpart hard limit; reserve a few chars for prefix
    // + separator + server suffix budget. We cap at 180 to stay safe.
    const MAX_LOCALPART_LEN: usize = 180;
    let session = sanitize_localpart_fragment(session_id);
    let label = sanitize_localpart_fragment(agent_label);
    let mut out = format!("{prefix}{session}_{label}");
    if out.len() > MAX_LOCALPART_LEN {
        out.truncate(MAX_LOCALPART_LEN);
        while !out.is_char_boundary(out.len()) {
            out.pop();
        }
    }
    out
}

fn room_alias_localpart(prefix: &str, session_id: &str) -> String {
    const MAX_ALIAS_LEN: usize = 180;
    let session = sanitize_localpart_fragment(session_id);
    let mut out = format!("{prefix}{session}");
    if out.len() > MAX_ALIAS_LEN {
        out.truncate(MAX_ALIAS_LEN);
        while !out.is_char_boundary(out.len()) {
            out.pop();
        }
    }
    out
}

fn record_swarm_room_action(action: &'static str) {
    metrics::counter!(
        "octos_matrix_swarm_room_total",
        "action" => action.to_string()
    )
    .increment(1);
}

/// Create a Matrix room via the Client-Server API using the appservice token.
///
/// Returns the created room ID, or the already-existing room ID if the
/// requested alias is taken (`M_ROOM_IN_USE`). The caller must resolve the
/// alias in that case because the Matrix createRoom response only echoes the
/// alias, not the room_id.
#[allow(clippy::too_many_arguments)]
async fn create_or_resolve_room(
    http: &reqwest::Client,
    homeserver: &str,
    as_token: &str,
    server_name: &str,
    alias_localpart: &str,
    name: &str,
    topic: &str,
    invite: &[String],
    creator_user_id: &str,
) -> Result<MatrixRoomId> {
    let url = format!(
        "{homeserver}/_matrix/client/v3/createRoom?user_id={}",
        percent_encode_path(creator_user_id),
    );
    let body = json!({
        "preset": "private_chat",
        "visibility": "private",
        "room_alias_name": alias_localpart,
        "name": name,
        "topic": topic,
        "invite": invite,
    });
    let resp = http
        .post(&url)
        .bearer_auth(as_token)
        .json(&body)
        .send()
        .await
        .wrap_err("failed to send createRoom request to homeserver")?;

    let status = resp.status();
    if status.is_success() {
        let resp_body: Value = resp
            .json()
            .await
            .wrap_err("failed to parse createRoom response")?;
        let room_id = resp_body
            .get("room_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre::eyre!("createRoom response missing room_id: {resp_body}"))?;
        return Ok(MatrixRoomId::new(room_id));
    }

    let resp_body: Value = resp
        .json()
        .await
        .unwrap_or_else(|_| json!({"errcode": "UNKNOWN"}));
    let errcode = resp_body
        .get("errcode")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if errcode == "M_ROOM_IN_USE" {
        // Alias is already taken — resolve it back to the existing room_id.
        let alias = format!("#{alias_localpart}:{server_name}");
        return resolve_room_alias(http, homeserver, as_token, &alias).await;
    }

    let error_msg = resp_body
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown error");
    Err(eyre::eyre!(
        "createRoom failed: status={status} errcode={errcode} error={error_msg}"
    ))
}

/// Resolve a Matrix room alias (`#foo:server`) to a room ID via the directory
/// API. Used when `M_ROOM_IN_USE` signals an idempotent create.
async fn resolve_room_alias(
    http: &reqwest::Client,
    homeserver: &str,
    as_token: &str,
    alias: &str,
) -> Result<MatrixRoomId> {
    let url = format!(
        "{homeserver}/_matrix/client/v3/directory/room/{}",
        percent_encode_path(alias),
    );
    let resp = http
        .get(&url)
        .bearer_auth(as_token)
        .send()
        .await
        .wrap_err("failed to resolve room alias")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(eyre::eyre!(
            "directory lookup failed for alias {alias}: {status} {body}"
        ));
    }
    let body: Value = resp
        .json()
        .await
        .wrap_err("failed to parse alias resolution")?;
    let room_id = body
        .get("room_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("alias resolution missing room_id: {body}"))?;
    Ok(MatrixRoomId::new(room_id))
}

/// Invite an already-registered user to a room using the appservice bot's
/// identity. Idempotent: `M_FORBIDDEN` ("already in room") is treated as
/// success.
async fn invite_user_to_room(
    http: &reqwest::Client,
    homeserver: &str,
    as_token: &str,
    room_id: &str,
    user_id: &str,
    inviter_user_id: &str,
) -> Result<()> {
    let url = format!(
        "{homeserver}/_matrix/client/v3/rooms/{}/invite?user_id={}",
        percent_encode_path(room_id),
        percent_encode_path(inviter_user_id),
    );
    let body = json!({ "user_id": user_id });
    let resp = http
        .post(&url)
        .bearer_auth(as_token)
        .json(&body)
        .send()
        .await
        .wrap_err("failed to send invite request")?;

    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }

    let resp_body: Value = resp
        .json()
        .await
        .unwrap_or_else(|_| json!({ "errcode": "UNKNOWN" }));
    let errcode = resp_body
        .get("errcode")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // User already in room is not a failure for idempotent ensure_swarm_room.
    if errcode == "M_FORBIDDEN" || errcode == "M_UNKNOWN" {
        let err_str = resp_body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if err_str.contains("already in the room") || err_str.contains("already a member") {
            return Ok(());
        }
    }
    Err(eyre::eyre!(
        "invite failed: room={room_id} user={user_id} status={status} body={resp_body}"
    ))
}

impl MatrixChannel {
    /// Attach the swarm supervisor contract.
    ///
    /// When this is called, the channel gains `register_subagent_puppet`,
    /// `ensure_swarm_room`, `route_subagent_event`, and the supervisor-reply
    /// matcher. Until this is called (or when the profile config omits the
    /// `matrix.swarm_supervisor` section), the channel behaves exactly like
    /// the pre-M7.3 appservice bot — no new routes, no puppet provisioning,
    /// no extra state. This enforces invariant 5.
    pub fn with_swarm_supervisor(mut self, params: SwarmSupervisorParams) -> Self {
        self.swarm_supervisor = Some(Arc::new(SwarmSupervisorState {
            puppet_prefix: params.puppet_prefix,
            room_prefix: params.room_prefix,
            homeserver: self.homeserver.clone(),
            as_token: self.as_token.clone(),
            server_name: self.server_name.clone(),
            supervisor_user_ids: params.supervisor_user_ids,
            rooms: Arc::new(RwLock::new(HashMap::new())),
            puppets: Arc::new(RwLock::new(HashMap::new())),
            http: self.http.clone(),
        }));
        self
    }

    /// Return the shared swarm supervisor state, if configured.
    fn swarm_state(&self) -> Result<Arc<SwarmSupervisorState>> {
        self.swarm_supervisor
            .as_ref()
            .cloned()
            .ok_or_else(|| eyre::eyre!("swarm supervisor not configured for this Matrix channel"))
    }

    /// Register a sub-agent as a Matrix puppet user. Idempotent.
    ///
    /// Invariant 1: re-registering the same `(session_id, agent_label)` is a
    /// no-op and returns the same [`MatrixUserId`]. The puppet is also added
    /// to the channel's `bot_router` + `registered_users` so outbound sends
    /// from the puppet identity pass the sender-registration check.
    pub async fn register_subagent_puppet(
        &self,
        session_id: &str,
        agent_label: &str,
    ) -> Result<MatrixUserId> {
        let state = self.swarm_state()?;
        let key = (session_id.to_string(), agent_label.to_string());

        // Fast path: already registered.
        if let Some(cached) = state.puppets.read().await.get(&key).cloned() {
            return Ok(cached);
        }

        let user_id = state.puppet_user_id_for(session_id, agent_label);
        let localpart = managed_localpart(user_id.as_str(), &state.server_name)
            .ok_or_else(|| eyre::eyre!("invalid puppet user_id: {user_id}"))?;

        // `register_user_via_appservice` treats `M_USER_IN_USE` as success, so
        // re-running this from another process (after a restart) is safe.
        register_user_via_appservice(&state.http, &state.homeserver, &state.as_token, localpart)
            .await
            .wrap_err_with(|| format!("failed to register puppet {user_id}"))?;

        // Profile ID mirrors `session_id--agent_label` so the existing
        // bot_router lookup surface works for messages addressed to this
        // puppet. The `owner` and `visibility` fields are not semantically
        // meaningful for supervisor puppets; we mark them public so existing
        // visibility gates in `handle_transaction` are a no-op.
        let profile_id = format!("{session_id}--{agent_label}");
        self.bot_router
            .register_entry(user_id.as_str(), &profile_id, "", BotVisibility::Public)
            .await?;
        self.registered_users
            .write()
            .await
            .insert(user_id.as_str().to_string());

        // Cache last so a mid-flight failure doesn't poison the map.
        state.puppets.write().await.insert(key, user_id.clone());
        Ok(user_id)
    }

    /// Ensure the per-swarm supervisor room exists, creating it if needed.
    /// Idempotent.
    ///
    /// Invariant 2: re-calling this for the same `session_id` returns the
    /// same [`MatrixRoomId`]. On first call we `createRoom` with a stable
    /// alias `#<room_prefix><session>:<server>`; on subsequent calls we hit
    /// the cache, and if the cache is cold after a restart we recover via
    /// `M_ROOM_IN_USE` → directory lookup.
    ///
    /// Every configured supervisor is invited at creation time. The
    /// appservice bot is implicitly the room creator and stays joined.
    pub async fn ensure_swarm_room(&self, session_id: &str) -> Result<MatrixRoomId> {
        let state = self.swarm_state()?;

        // Fast path: cached.
        if let Some(cached) = state.rooms.read().await.get(session_id).cloned() {
            return Ok(cached);
        }

        let alias_localpart = state.room_alias_localpart(session_id);
        let room = create_or_resolve_room(
            &state.http,
            &state.homeserver,
            &state.as_token,
            &state.server_name,
            &alias_localpart,
            &format!("Swarm {session_id}"),
            &format!("octos swarm supervisor — session={session_id}"),
            &state.supervisor_user_ids,
            &self.bot_user_id,
        )
        .await?;

        record_swarm_room_action("created");

        // Invite supervisors best-effort (they may be invited by createRoom
        // already). This cleans up races where createRoom returned an
        // already-existing room whose supervisor list drifted.
        for supervisor in &state.supervisor_user_ids {
            if let Err(e) = invite_user_to_room(
                &state.http,
                &state.homeserver,
                &state.as_token,
                room.as_str(),
                supervisor,
                &self.bot_user_id,
            )
            .await
            {
                warn!(
                    session_id,
                    supervisor, error = %e,
                    "failed to invite supervisor to swarm room (non-fatal)"
                );
            }
        }

        state
            .rooms
            .write()
            .await
            .insert(session_id.to_string(), room.clone());
        Ok(room)
    }

    /// Route a typed harness event to the addressed puppet's message in the
    /// swarm room.
    ///
    /// Invariant 3: serialization preserves `kind` + key summary fields. The
    /// Matrix `body` carries the human-readable one-liner; `formatted_body`
    /// carries the JSON envelope inside `<pre>` so clients that render HTML
    /// can show the structured payload. `msgtype` is `m.text`.
    ///
    /// Prerequisites: both the puppet and the room must have been ensured
    /// via [`register_subagent_puppet`](Self::register_subagent_puppet) +
    /// [`ensure_swarm_room`](Self::ensure_swarm_room). Callers can rely on
    /// those being idempotent, so a simple "ensure then route" pattern is
    /// safe to run on every event.
    pub async fn route_subagent_event(
        &self,
        session_id: &str,
        agent_label: &str,
        event: SwarmHarnessEvent,
    ) -> Result<MatrixEventId> {
        let state = self.swarm_state()?;
        let puppet = state.puppet_user_id_for(session_id, agent_label);
        let room = state
            .rooms
            .read()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| {
                eyre::eyre!(
                    "swarm room not ensured for session {session_id}; call ensure_swarm_room first"
                )
            })?;

        let summary = event.summary();
        let envelope = json!({
            "schema": SWARM_SUPERVISOR_EVENT_SCHEMA_V1,
            "kind": event.kind(),
            "agent_label": agent_label,
            "session_id": session_id,
            "event": serde_json::to_value(&event)
                .wrap_err("failed to serialize swarm harness event")?,
        });
        let envelope_pretty = serde_json::to_string_pretty(&envelope)
            .wrap_err("failed to render swarm harness event")?;
        let event_id = self
            .send_swarm_room_message(
                room.as_str(),
                puppet.as_str(),
                &summary,
                &envelope_pretty,
                &envelope,
            )
            .await?;

        record_swarm_room_action("routed");
        Ok(event_id)
    }

    async fn send_swarm_room_message(
        &self,
        room_id: &str,
        sender_user_id: &str,
        body: &str,
        pretty_envelope: &str,
        envelope_value: &Value,
    ) -> Result<MatrixEventId> {
        let txn_id = uuid::Uuid::now_v7().to_string();
        let path = format!(
            "/_matrix/client/v3/rooms/{}/send/m.room.message/{}?user_id={}",
            percent_encode_path(room_id),
            percent_encode_path(&txn_id),
            percent_encode_path(sender_user_id),
        );
        let url = format!("{}{}", self.homeserver, path);
        // Escape `<`, `>`, `&` for HTML pre-block.
        let escaped = pretty_envelope
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        let formatted_body = format!("<pre><code class=\"language-json\">{escaped}</code></pre>");
        let body_payload = json!({
            "msgtype": MSGTYPE_TEXT,
            "body": body,
            "format": HTML_FORMAT,
            "formatted_body": formatted_body,
            // Structured envelope — clients that understand the custom event
            // type can render the event directly without parsing the pre
            // block. This mirrors the `m.new_content` convention.
            "org.octos.swarm_event": envelope_value,
        });
        let resp = self
            .http
            .put(&url)
            .bearer_auth(&self.as_token)
            .json(&body_payload)
            .send()
            .await
            .wrap_err("failed to send swarm harness event to Matrix")?;
        let status = resp.status();
        let resp_body: Value = resp
            .json()
            .await
            .wrap_err("failed to parse Matrix send response")?;
        if !status.is_success() {
            return Err(eyre::eyre!(
                "Matrix send failed for swarm event: status={status} body={resp_body}"
            ));
        }
        let event_id = resp_body
            .get("event_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre::eyre!("Matrix send response missing event_id: {resp_body}"))?
            .to_string();
        Ok(MatrixEventId(event_id))
    }

    /// Inspect a supervisor's reply and route it to the addressed puppet.
    ///
    /// Invariant 4: the reply is routed ONLY to the puppet explicitly
    /// addressed — either via `@puppet:server` text mention, a Matrix
    /// `m.mentions` entry, or an explicit `m.relates_to.m.in_reply_to`
    /// (fallback) where the referenced event was sent by a known puppet.
    /// Replies that do not address exactly one puppet return `None` — the
    /// caller MUST NOT broadcast to all puppets.
    ///
    /// Returns `None` when:
    /// - the sender is not a configured supervisor AND is not the channel's
    ///   operator sender (keeps unrelated room traffic out of steering);
    /// - the room is not a known swarm room;
    /// - the reply addresses zero puppets or more than one puppet.
    pub async fn handle_supervisor_reply(
        &self,
        room_id: &str,
        sender: &str,
        message: &str,
    ) -> Option<SteeringInput> {
        let state = self.swarm_supervisor.clone()?;

        // Find the session this room belongs to. This also filters out
        // messages from non-swarm rooms.
        let session_id = {
            let rooms = state.rooms.read().await;
            rooms
                .iter()
                .find(|(_, room)| room.as_str() == room_id)
                .map(|(session, _)| session.clone())
        }?;

        // Supervisors are the configured `supervisor_user_ids` plus any
        // operator sender (matching the existing admin-sender gate).
        let is_configured_supervisor = state.supervisor_user_ids.iter().any(|u| u == sender);
        let is_operator = self.is_operator_sender(sender);
        if !is_configured_supervisor && !is_operator {
            return None;
        }

        // Collect puppets registered for this session.
        let candidates: Vec<(String, MatrixUserId)> = {
            let puppets = state.puppets.read().await;
            puppets
                .iter()
                .filter(|((sess, _), _)| sess == &session_id)
                .map(|((_, label), uid)| (label.clone(), uid.clone()))
                .collect()
        };
        if candidates.is_empty() {
            return None;
        }

        // Invariant 4: address exactly one puppet.
        //
        // We recognize three mention forms common in Matrix clients:
        //   1. Text: `@puppet:server body` (Element autocomplete pill)
        //   2. Text: `@puppet:server: body` (humans typing the classic prefix)
        //   3. HTML pill: `<a href="https://matrix.to/#/@puppet:server">…</a>`
        // `contains_puppet_mention` relaxes the trailing-char rule to accept
        // `:` + whitespace, which the baseline mention matcher rejects.
        let matches: Vec<&(String, MatrixUserId)> = candidates
            .iter()
            .filter(|(_, uid)| contains_puppet_mention(message, uid.as_str()))
            .collect();

        if matches.len() != 1 {
            return None;
        }

        let (agent_label, puppet_user_id) = matches[0].clone();
        let stripped = strip_puppet_mention(message, puppet_user_id.as_str());

        record_swarm_room_action("replied");
        Some(SteeringInput {
            session_id,
            agent_label,
            puppet_user_id,
            supervisor_user_id: sender.to_string(),
            body: stripped,
        })
    }
}

/// Whether the first character after a matched puppet user_id is allowed to
/// end the mention.
///
/// Unlike [`is_matrix_user_id_char`], this accepts the `:` that humans type
/// in the classic `@puppet:server: body` prefix — once a complete
/// `@localpart:server_name` has matched, a second `:` cannot be part of the
/// same user_id and so terminates the mention.
fn is_puppet_mention_end(c: char) -> bool {
    !is_matrix_user_id_char(c) || c == ':'
}

/// Variant of [`contains_exact_matrix_user_id_mention`] tailored to the
/// supervisor reply matcher. See [`is_puppet_mention_end`] for the relaxed
/// terminal rule.
fn contains_puppet_mention(text: &str, user_id: &str) -> bool {
    find_puppet_mention(text, user_id).is_some()
}

fn find_puppet_mention(text: &str, user_id: &str) -> Option<(usize, usize)> {
    for (idx, _) in text.match_indices(user_id) {
        let start_ok = text[..idx]
            .chars()
            .next_back()
            .is_none_or(|c| !is_matrix_user_id_char(c));
        let end_idx = idx + user_id.len();
        let end_ok = text[end_idx..]
            .chars()
            .next()
            .is_none_or(is_puppet_mention_end);
        if start_ok && end_ok {
            return Some((idx, end_idx));
        }
    }
    None
}

/// Strip the first occurrence of `@puppet:server` (plus optional trailing
/// colon/space) from a supervisor reply so the downstream agent sees a clean
/// message body.
fn strip_puppet_mention(message: &str, user_id: &str) -> String {
    let Some((idx, end_idx)) = find_puppet_mention(message, user_id) else {
        return message.trim().to_string();
    };
    let mut prefix = message[..idx].to_string();
    let suffix = &message[end_idx..];
    // Strip a leading `:` and any whitespace the mention was followed by so
    // the classic `@foo: do X` pattern routes cleanly.
    let tail = suffix
        .trim_start_matches(|c: char| c == ':' || c.is_whitespace())
        .to_string();
    if prefix.ends_with(char::is_whitespace) {
        prefix = prefix.trim_end().to_string();
    }
    let mut out = String::with_capacity(prefix.len() + tail.len() + 1);
    out.push_str(prefix.trim_end());
    if !prefix.is_empty() && !tail.is_empty() {
        out.push(' ');
    }
    out.push_str(&tail);
    out.trim().to_string()
}

/// A Matrix event ID newtype — the return value of a successful send.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MatrixEventId(String);

impl MatrixEventId {
    pub fn new(event_id: impl Into<String>) -> Self {
        Self(event_id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MatrixEventId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
#[path = "matrix_channel_tests.rs"]
mod tests;
