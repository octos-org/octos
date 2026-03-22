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
const MEMBERSHIP_INVITE: &str = "invite";
const REL_TYPE_REPLACE: &str = "m.replace";
const HTML_FORMAT: &str = "org.matrix.custom.html";
const METADATA_TARGET_PROFILE_ID: &str = "target_profile_id";
const METADATA_TARGET_MATRIX_USER_ID: &str = "target_matrix_user_id";
const CONTENT_TARGET_USER_ID: &str = "org.octos.target_user_id";
const CONTENT_TARGET_USER_ID_LEGACY: &str = "target_user_id";
#[cfg(not(test))]
const MAX_EVENT_SENDER_CACHE: usize = 2048;
#[cfg(test)]
const MAX_EVENT_SENDER_CACHE: usize = 4;

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
        routes.get(matrix_user_id).map(|entry| entry.profile_id.clone())
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
        }
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
fn percent_encode_path(s: &str) -> String {
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
                            if entry.visibility == BotVisibility::Private && inviter != entry.owner {
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
        if msgtype != MSGTYPE_TEXT {
            continue;
        }

        let body_text = content.get("body").and_then(|v| v.as_str()).unwrap_or("");
        if body_text.is_empty() {
            continue;
        }

        // Intercept slash commands before routing to agent
        if let Some(response) = handle_slash_command(&state, sender, room_id, body_text).await {
            if let Err(e) = send_text_to_room(&state, room_id, &response).await {
                warn!(error = %e, room_id, "failed to send slash command response");
            }
            continue;
        }

        let event_id = event
            .get("event_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Route to bot profile: explicit target first, then @mention, then DM room mapping
        let mut metadata = json!({});
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

        let inbound = InboundMessage {
            channel: CHANNEL_NAME.into(),
            sender_id: sender.to_string(),
            chat_id: room_id.to_string(),
            content: body_text.to_string(),
            timestamp: Utc::now(),
            media: vec![],
            metadata,
            message_id: event_id,
        };

        if state.inbound_tx.send(inbound).await.is_err() {
            warn!("inbound channel closed while processing Matrix transaction");
            break;
        }
    }

    (StatusCode::OK, "{}").into_response()
}

// ── Slash command handling ───────────────────────────────────────────────────

/// Check if a message is a slash command and handle it.
/// Returns `Some(response_text)` if it was a slash command, `None` otherwise.
async fn handle_slash_command(
    state: &AppserviceState,
    sender: &str,
    _room_id: &str,
    body: &str,
) -> Option<String> {
    let bot_manager = state.bot_manager.as_ref()?;

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
• `/bothelp`";

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

    let prompt = if after.starts_with('"') {
        // Find closing quote
        if let Some(end) = after[1..].find('"') {
            Some(after[1..1 + end].to_string())
        } else {
            // No closing quote — take everything after the opening quote
            Some(after[1..].to_string())
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

        let event_id = self
            .send_matrix_message(&msg.chat_id, &msg.content, sender_user_id)
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
        // Use the same sender identity that sent the original message.
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
        let body = json!({
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
                "Matrix edit_message failed: status={status} body={resp_body}"
            ));
        }

        Ok(())
    }

    async fn send_typing(&self, chat_id: &str) -> Result<()> {
        self.send_typing_as(chat_id, None).await
    }

    async fn send_typing_as(&self, chat_id: &str, sender_user_id: Option<&str>) -> Result<()> {
        let sender = sender_user_id.unwrap_or(&self.bot_user_id);
        let url = self.make_api_url(&format!(
            "/_matrix/client/v3/rooms/{}/typing/{}?user_id={}",
            percent_encode_path(chat_id),
            percent_encode_path(sender),
            percent_encode_path(sender),
        ));

        let body = json!({
            "typing": true,
            "timeout": 30000,
        });

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
                "typing indicator request returned non-success"
            );
        }

        Ok(())
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
    /// Send a message to a Matrix room and return the event_id.
    ///
    /// If `sender_user_id` is `Some`, the request uses that user for identity
    /// assertion (`?user_id=`); otherwise the default `bot_user_id` is used.
    async fn send_matrix_message(
        &self,
        room_id: &str,
        content: &str,
        sender_user_id: Option<&str>,
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
        let body = json!({
            "msgtype": MSGTYPE_TEXT,
            "body": content,
            "format": HTML_FORMAT,
            "formatted_body": formatted_body,
        });

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use axum::extract::State;
    use axum::http::{Method, Uri};
    use axum::routing::any;
    use tokio::sync::Mutex;

    fn make_channel() -> MatrixChannel {
        MatrixChannel::new(
            "http://localhost:6167",
            "as_token_test",
            "hs_token_test",
            "localhost",
            "octos_bot",
            "octos_",
            9880,
            Arc::new(AtomicBool::new(false)),
        )
    }

    fn make_test_state(inbound_tx: mpsc::Sender<InboundMessage>) -> AppserviceState {
        let mut registered = HashSet::new();
        registered.insert("@octos_bot:localhost".to_string());
        AppserviceState {
            inbound_tx,
            homeserver: "http://localhost:6167".to_string(),
            as_token: "test_as_token".to_string(),
            hs_token: "test_token".to_string(),
            bot_user_id: "@octos_bot:localhost".to_string(),
            server_name: "localhost".to_string(),
            user_prefix: "octos_".to_string(),
            http: reqwest::Client::new(),
            registered_users: Arc::new(RwLock::new(registered)),
            dedup: Arc::new(MessageDedup::new()),
            bot_router: Arc::new(BotRouter::new(None)),
            bot_manager: None,
        }
    }

    #[derive(Clone, Debug)]
    struct CapturedRequest {
        method: Method,
        path: String,
        query: Option<String>,
        body: Value,
    }

    #[derive(Clone)]
    struct MockHomeserverState {
        requests: Arc<Mutex<Vec<CapturedRequest>>>,
        status: StatusCode,
        response_body: Value,
    }

    async fn capture_homeserver_request(
        State(state): State<MockHomeserverState>,
        method: Method,
        uri: Uri,
        body: String,
    ) -> impl IntoResponse {
        let body = if body.is_empty() {
            json!({})
        } else {
            serde_json::from_str(&body).unwrap_or_else(|_| json!({ "raw": body }))
        };
        state.requests.lock().await.push(CapturedRequest {
            method,
            path: uri.path().to_string(),
            query: uri.query().map(str::to_string),
            body,
        });
        (
            state.status,
            serde_json::to_string(&state.response_body).unwrap(),
        )
    }

    async fn spawn_mock_homeserver() -> (
        String,
        Arc<Mutex<Vec<CapturedRequest>>>,
        tokio::task::JoinHandle<()>,
    ) {
        spawn_mock_homeserver_with_response(StatusCode::OK, json!({"event_id":"$test_event"})).await
    }

    async fn spawn_mock_homeserver_with_response(
        status: StatusCode,
        response_body: Value,
    ) -> (
        String,
        Arc<Mutex<Vec<CapturedRequest>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = MockHomeserverState {
            requests: requests.clone(),
            status,
            response_body,
        };
        let app = Router::new()
            .route(
                "/_matrix/client/v3/register",
                any(capture_homeserver_request),
            )
            .route(
                "/_matrix/client/v3/account/whoami",
                any(capture_homeserver_request),
            )
            .route(
                "/_matrix/client/v3/rooms/{room_id}/join",
                any(capture_homeserver_request),
            )
            .route(
                "/_matrix/client/v3/rooms/{room_id}/leave",
                any(capture_homeserver_request),
            )
            .route(
                "/_matrix/client/v3/rooms/{room_id}/send/{event_type}/{txn_id}",
                any(capture_homeserver_request),
            )
            .route(
                "/_matrix/client/v3/rooms/{room_id}/typing/{user_id}",
                any(capture_homeserver_request),
            )
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{}", addr), requests, handle)
    }

    async fn spawn_mock_homeserver_with_dynamic_event_ids() -> (
        String,
        Arc<Mutex<Vec<CapturedRequest>>>,
        tokio::task::JoinHandle<()>,
    ) {
        async fn dynamic_handler(
            State(requests): State<Arc<Mutex<Vec<CapturedRequest>>>>,
            method: Method,
            uri: Uri,
            body: String,
        ) -> impl IntoResponse {
            let body = if body.is_empty() {
                json!({})
            } else {
                serde_json::from_str(&body).unwrap_or_else(|_| json!({ "raw": body }))
            };
            requests.lock().await.push(CapturedRequest {
                method,
                path: uri.path().to_string(),
                query: uri.query().map(str::to_string),
                body,
            });
            let event_id = uri
                .path()
                .rsplit('/')
                .next()
                .map(|txn_id| format!("${txn_id}"))
                .unwrap_or_else(|| "$missing_txn".to_string());
            (
                StatusCode::OK,
                serde_json::to_string(&json!({ "event_id": event_id })).unwrap(),
            )
        }

        let requests = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route(
                "/_matrix/client/v3/rooms/{room_id}/send/{event_type}/{txn_id}",
                any(dynamic_handler),
            )
            .with_state(requests.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{}", addr), requests, handle)
    }

    fn unused_local_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    async fn wait_for_request_count(requests: &Arc<Mutex<Vec<CapturedRequest>>>, min_count: usize) {
        for _ in 0..20 {
            if requests.lock().await.len() >= min_count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    #[test]
    fn test_matrix_channel_name() {
        let ch = make_channel();
        assert_eq!(ch.name(), "matrix");
    }

    #[test]
    fn test_matrix_supports_edit() {
        let ch = make_channel();
        assert!(ch.supports_edit());
    }

    #[test]
    fn test_matrix_max_message_length() {
        let ch = make_channel();
        assert_eq!(ch.max_message_length(), 65535);
    }

    #[test]
    fn test_matrix_bot_user_id() {
        let ch = make_channel();
        assert_eq!(ch.bot_user_id(), "@octos_bot:localhost");
    }

    #[test]
    fn test_make_api_url() {
        let ch = make_channel();
        assert_eq!(
            ch.make_api_url("/_matrix/client/v3/account/whoami"),
            "http://localhost:6167/_matrix/client/v3/account/whoami"
        );
    }

    #[test]
    fn test_make_api_url_strips_trailing_slash() {
        let ch = MatrixChannel::new(
            "http://localhost:6167/",
            "as",
            "hs",
            "localhost",
            "bot",
            "octos_",
            9880,
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(
            ch.make_api_url("/_matrix/client/v3/whoami"),
            "http://localhost:6167/_matrix/client/v3/whoami"
        );
    }

    #[test]
    fn test_default_appservice_bind_addr_uses_all_interfaces() {
        assert_eq!(default_appservice_bind_addr(9880), "0.0.0.0:9880");
    }

    #[test]
    fn test_is_managed_user_bot() {
        assert!(is_managed_user(
            "@octos_bot:localhost",
            "@octos_bot:localhost",
            ":localhost",
            "octos_",
        ));
    }

    #[test]
    fn test_is_managed_user_virtual_user() {
        assert!(is_managed_user(
            "@octos_agent1:localhost",
            "@octos_bot:localhost",
            ":localhost",
            "octos_",
        ));
    }

    #[test]
    fn test_is_managed_user_regular_user() {
        assert!(!is_managed_user(
            "@alice:localhost",
            "@octos_bot:localhost",
            ":localhost",
            "octos_",
        ));
    }

    #[test]
    fn test_is_managed_user_other_server() {
        assert!(!is_managed_user(
            "@octos_bot:other.server",
            "@octos_bot:localhost",
            ":localhost",
            "octos_",
        ));
    }

    // ── Token validation tests ───────────────────────────────────────────

    #[test]
    fn test_validate_hs_token_query_valid() {
        let query = AccessTokenQuery {
            access_token: Some("secret".to_string()),
        };
        let headers = HeaderMap::new();
        assert!(validate_hs_token(&query, &headers, "secret").is_ok());
    }

    #[test]
    fn test_validate_hs_token_query_invalid() {
        let query = AccessTokenQuery {
            access_token: Some("wrong".to_string()),
        };
        let headers = HeaderMap::new();
        let result = validate_hs_token(&query, &headers, "secret");
        assert_eq!(result.unwrap_err(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn test_validate_hs_token_bearer_valid() {
        let query = AccessTokenQuery { access_token: None };
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer secret".parse().unwrap());
        assert!(validate_hs_token(&query, &headers, "secret").is_ok());
    }

    #[test]
    fn test_validate_hs_token_bearer_invalid() {
        let query = AccessTokenQuery { access_token: None };
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer wrong".parse().unwrap());
        let result = validate_hs_token(&query, &headers, "secret");
        assert_eq!(result.unwrap_err(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn test_validate_hs_token_missing() {
        let query = AccessTokenQuery { access_token: None };
        let headers = HeaderMap::new();
        let result = validate_hs_token(&query, &headers, "secret");
        assert_eq!(result.unwrap_err(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn test_validate_hs_token_rejects_mismatched_query_and_header() {
        let query = AccessTokenQuery {
            access_token: Some("secret".to_string()),
        };
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer wrong".parse().unwrap());
        let result = validate_hs_token(&query, &headers, "secret");
        assert_eq!(result.unwrap_err(), StatusCode::FORBIDDEN);
    }

    // ── txn_id dedup test ────────────────────────────────────────────────

    #[test]
    fn test_txn_id_dedup() {
        let ch = make_channel();

        // First time: not a duplicate
        assert!(!ch.dedup.is_duplicate("txn_1"));

        // Second time: duplicate
        assert!(ch.dedup.is_duplicate("txn_1"));
    }

    // ── Registered users test ────────────────────────────────────────────

    #[tokio::test]
    async fn test_registered_users() {
        let ch = make_channel();
        {
            let mut users = ch.registered_users.write().await;
            users.insert("@octos_bot:localhost".to_string());
        }
        let users = ch.registered_users.read().await;
        assert!(users.contains("@octos_bot:localhost"));
        assert!(!users.contains("@other:localhost"));
    }

    // ── Axum handler integration tests ───────────────────────────────────

    #[tokio::test]
    async fn test_handle_transaction_missing_token() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (inbound_tx, _inbound_rx) = mpsc::channel::<InboundMessage>(16);

        let mut state = make_test_state(inbound_tx);
        state.hs_token = "correct_token".to_string();

        let app = Router::new()
            .route(
                "/_matrix/app/v1/transactions/{txn_id}",
                put(handle_transaction),
            )
            .with_state(state);

        let body = json!({"events": []});

        let req = Request::builder()
            .method("PUT")
            .uri("/_matrix/app/v1/transactions/txn1")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_handle_transaction_bad_json_does_not_poison_txn_id() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);

        let state = make_test_state(inbound_tx);

        let app = Router::new()
            .route(
                "/_matrix/app/v1/transactions/{txn_id}",
                put(handle_transaction),
            )
            .with_state(state);

        let bad_req = Request::builder()
            .method("PUT")
            .uri("/_matrix/app/v1/transactions/txn_retry?access_token=test_token")
            .header("content-type", "application/json")
            .body(Body::from("{not-json"))
            .unwrap();

        let bad_resp = app.clone().oneshot(bad_req).await.unwrap();
        assert_eq!(bad_resp.status(), StatusCode::BAD_REQUEST);

        let good_body = json!({
            "events": [{
                "type": "m.room.message",
                "sender": "@alice:elsewhere.org",
                "room_id": "!room:localhost",
                "event_id": "$ev_retry",
                "content": {
                    "msgtype": "m.text",
                    "body": "retry should still deliver"
                }
            }]
        });
        let good_req = Request::builder()
            .method("PUT")
            .uri("/_matrix/app/v1/transactions/txn_retry?access_token=test_token")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&good_body).unwrap()))
            .unwrap();

        let good_resp = app.oneshot(good_req).await.unwrap();
        assert_eq!(good_resp.status(), StatusCode::OK);

        let msg = inbound_rx.try_recv().unwrap();
        assert_eq!(msg.content, "retry should still deliver");
    }

    #[tokio::test]
    async fn test_handle_transaction_ignores_bot_messages() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);

        let state = make_test_state(inbound_tx);

        let app = Router::new()
            .route(
                "/_matrix/app/v1/transactions/{txn_id}",
                put(handle_transaction),
            )
            .with_state(state);

        let body = json!({
            "events": [{
                "type": "m.room.message",
                "sender": "@octos_bot:localhost",
                "room_id": "!room:localhost",
                "event_id": "$ev_bot",
                "content": {
                    "msgtype": "m.text",
                    "body": "bot's own message"
                }
            }]
        });

        let req = Request::builder()
            .method("PUT")
            .uri("/_matrix/app/v1/transactions/txn_bot?access_token=test_token")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Bot's own message should be ignored
        assert!(inbound_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_handle_ping() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (inbound_tx, _) = mpsc::channel::<InboundMessage>(16);
        let state = make_test_state(inbound_tx);
        let app = Router::new()
            .route("/_matrix/app/v1/ping", axum::routing::post(handle_ping))
            .with_state(state);

        let req = Request::builder()
            .method("POST")
            .uri("/_matrix/app/v1/ping?access_token=test_token")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_handle_ping_requires_token() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (inbound_tx, _) = mpsc::channel::<InboundMessage>(16);
        let state = make_test_state(inbound_tx);
        let app = Router::new()
            .route("/_matrix/app/v1/ping", axum::routing::post(handle_ping))
            .with_state(state);

        let req = Request::builder()
            .method("POST")
            .uri("/_matrix/app/v1/ping")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_handle_user_query_bot() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (inbound_tx, _) = mpsc::channel::<InboundMessage>(16);

        let state = make_test_state(inbound_tx);
        state
            .registered_users
            .write()
            .await
            .insert("@octos_agent1:localhost".to_string());

        let app = Router::new()
            .route("/_matrix/app/v1/users/{user_id}", get(handle_user_query))
            .with_state(state);

        // Query for bot user — should return 200
        let req = Request::builder()
            .method("GET")
            .uri("/_matrix/app/v1/users/@octos_bot:localhost?access_token=test_token")
            .body(Body::empty())
            .unwrap();

        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Query for virtual user — should return 200
        let req2 = Request::builder()
            .method("GET")
            .uri("/_matrix/app/v1/users/@octos_agent1:localhost?access_token=test_token")
            .body(Body::empty())
            .unwrap();

        let resp2 = app.clone().oneshot(req2).await.unwrap();
        assert_eq!(resp2.status(), StatusCode::OK);

        // Query for unknown user — should return 404
        let req3 = Request::builder()
            .method("GET")
            .uri("/_matrix/app/v1/users/@alice:localhost?access_token=test_token")
            .body(Body::empty())
            .unwrap();

        let resp3 = app.oneshot(req3).await.unwrap();
        assert_eq!(resp3.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_handle_user_query_unknown_managed_user_returns_404() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (homeserver, requests, homeserver_handle) = spawn_mock_homeserver().await;
        let (inbound_tx, _) = mpsc::channel::<InboundMessage>(16);
        let mut state = make_test_state(inbound_tx);
        state.homeserver = homeserver;
        state.as_token = "test_as_token".to_string();

        let app = Router::new()
            .route("/_matrix/app/v1/users/{user_id}", get(handle_user_query))
            .with_state(state);

        let req = Request::builder()
            .method("GET")
            .uri("/_matrix/app/v1/users/@octos_unknown:localhost?access_token=test_token")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        tokio::time::sleep(Duration::from_millis(50)).await;
        let requests = requests.lock().await;
        assert!(
            requests.is_empty(),
            "unknown managed user query should not auto-register with homeserver"
        );

        homeserver_handle.abort();
    }

    #[tokio::test]
    async fn test_handle_room_query_requires_token() {
        let appservice_port = unused_local_port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let channel = Arc::new(MatrixChannel::new(
            "http://localhost:6167",
            "as_token_test",
            "hs_token_test",
            "localhost",
            "octos_bot",
            "octos_",
            appservice_port,
            shutdown,
        ));

        let (inbound_tx, _inbound_rx) = mpsc::channel::<InboundMessage>(16);
        let channel_task = {
            let channel = channel.clone();
            tokio::spawn(async move { channel.start(inbound_tx).await.unwrap() })
        };

        tokio::time::sleep(Duration::from_millis(100)).await;
        let client = reqwest::Client::new();
        let resp = client
            .get(format!(
                "http://127.0.0.1:{appservice_port}/_matrix/app/v1/rooms/%23alias%3Alocalhost"
            ))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        channel.stop().await.unwrap();
        channel_task.await.unwrap();
    }

    #[tokio::test]
    async fn test_handle_transaction_invite_joins_room() {
        let (homeserver, requests, homeserver_handle) = spawn_mock_homeserver().await;
        let appservice_port = unused_local_port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let channel = Arc::new(MatrixChannel::new(
            &homeserver,
            "as_token_test",
            "hs_token_test",
            "localhost",
            "octos_bot",
            "octos_",
            appservice_port,
            shutdown,
        ));

        let (inbound_tx, _inbound_rx) = mpsc::channel::<InboundMessage>(16);
        let channel_task = {
            let channel = channel.clone();
            tokio::spawn(async move { channel.start(inbound_tx).await.unwrap() })
        };

        wait_for_request_count(&requests, 1).await;

        let body = json!({
            "events": [{
                "type": "m.room.member",
                "room_id": "!room123:localhost",
                "state_key": "@octos_agent1:localhost",
                "content": {
                    "membership": "invite"
                }
            }]
        });

        let client = reqwest::Client::new();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let http_resp = client
            .put(format!(
                "http://127.0.0.1:{appservice_port}/_matrix/app/v1/transactions/txn-invite?access_token=hs_token_test"
            ))
            .header("content-type", "application/json")
            .body(serde_json::to_string(&body).unwrap())
            .send()
            .await
            .unwrap();
        assert_eq!(http_resp.status(), StatusCode::OK);

        wait_for_request_count(&requests, 2).await;
        let requests = requests.lock().await;
        assert!(requests.iter().any(|req| {
            req.method == Method::POST
                && req.path == "/_matrix/client/v3/rooms/%21room123%3Alocalhost/join"
                && req
                    .query
                    .as_deref()
                    .is_some_and(|q| q.contains("user_id=%40octos_agent1%3Alocalhost"))
        }));

        channel.stop().await.unwrap();
        channel_task.await.unwrap();
        homeserver_handle.abort();
    }

    #[tokio::test]
    async fn test_private_bot_invite_rejected_for_non_owner() {
        let (homeserver, requests, homeserver_handle) = spawn_mock_homeserver().await;
        let (inbound_tx, _inbound_rx) = mpsc::channel::<InboundMessage>(16);
        let mut state = make_test_state(inbound_tx);
        state.homeserver = homeserver;

        let router = BotRouter::new(None);
        router
            .register_entry(
                "@octos_private:localhost",
                "main--private",
                "@owner:localhost",
                BotVisibility::Private,
            )
            .await
            .unwrap();
        state.bot_router = Arc::new(router);

        let app = Router::new()
            .route(
                "/_matrix/app/v1/transactions/{txn_id}",
                put(handle_transaction),
            )
            .with_state(state.clone());

        let body = json!({
            "events": [{
                "type": "m.room.member",
                "sender": "@mallory:localhost",
                "room_id": "!room123:localhost",
                "state_key": "@octos_private:localhost",
                "content": {
                    "membership": "invite"
                }
            }]
        });

        let req = axum::http::Request::builder()
            .method("PUT")
            .uri("/_matrix/app/v1/transactions/txn-private-invite?access_token=test_token")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();

        let resp = tower::ServiceExt::oneshot(app, req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        wait_for_request_count(&requests, 3).await;
        let requests = requests.lock().await;
        assert!(requests.iter().any(|req| req.path.ends_with("/join")));
        assert!(requests.iter().any(|req| req.path.contains("/send/")));
        assert!(requests.iter().any(|req| req.path.ends_with("/leave")));
        assert_eq!(
            state.bot_router.route_by_room("!room123:localhost").await,
            None,
            "private bot should not persist room mapping for non-owner invite"
        );

        homeserver_handle.abort();
    }

    #[tokio::test]
    async fn test_health_check_includes_user_id() {
        let (homeserver, requests, homeserver_handle) = spawn_mock_homeserver().await;
        let ch = MatrixChannel::new(
            &homeserver,
            "as_token_test",
            "hs_token_test",
            "localhost",
            "octos_bot",
            "octos_",
            9880,
            Arc::new(AtomicBool::new(false)),
        );

        let health = ch.health_check().await.unwrap();
        assert_eq!(health, ChannelHealth::Healthy);

        wait_for_request_count(&requests, 1).await;
        let requests = requests.lock().await;
        assert!(requests.iter().any(|req| {
            req.path == "/_matrix/client/v3/account/whoami"
                && req
                    .query
                    .as_deref()
                    .is_some_and(|q| q.contains("user_id=%40octos_bot%3Alocalhost"))
        }));

        homeserver_handle.abort();
    }

    #[tokio::test]
    async fn test_stop_sets_shutdown() {
        let ch = make_channel();
        assert!(!ch.shutdown.load(Ordering::Acquire));
        ch.stop().await.unwrap();
        assert!(ch.shutdown.load(Ordering::Acquire));
    }

    #[test]
    fn test_matrix_supports_edit_true() {
        let ch = make_channel();
        assert!(ch.supports_edit());
    }

    #[tokio::test]
    async fn test_matrix_appservice_receives_message() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
        let state = make_test_state(inbound_tx);

        let app = Router::new()
            .route(
                "/_matrix/app/v1/transactions/{txn_id}",
                put(handle_transaction),
            )
            .with_state(state);

        let body = json!({
            "events": [{
                "type": "m.room.message",
                "sender": "@alice:elsewhere.org",
                "room_id": "!room123:localhost",
                "event_id": "$event1",
                "content": {
                    "msgtype": "m.text",
                    "body": "hello from matrix"
                }
            }]
        });

        let req = Request::builder()
            .method("PUT")
            .uri("/_matrix/app/v1/transactions/txn1?access_token=test_token")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let msg = inbound_rx.try_recv().unwrap();
        assert_eq!(msg.channel, "matrix");
        assert_eq!(msg.sender_id, "@alice:elsewhere.org");
        assert_eq!(msg.chat_id, "!room123:localhost");
        assert_eq!(msg.content, "hello from matrix");
    }

    #[tokio::test]
    async fn test_matrix_rejects_invalid_hs_token() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (inbound_tx, _) = mpsc::channel::<InboundMessage>(16);
        let mut state = make_test_state(inbound_tx);
        state.hs_token = "correct_token".to_string();

        let app = Router::new()
            .route(
                "/_matrix/app/v1/transactions/{txn_id}",
                put(handle_transaction),
            )
            .with_state(state);

        let req = Request::builder()
            .method("PUT")
            .uri("/_matrix/app/v1/transactions/txn1?access_token=wrong_token")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"events":[]}"#))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn test_matrix_dedup_txn_id() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
        let state = make_test_state(inbound_tx);

        let app = Router::new()
            .route(
                "/_matrix/app/v1/transactions/{txn_id}",
                put(handle_transaction),
            )
            .with_state(state);

        let body = json!({
            "events": [{
                "type": "m.room.message",
                "sender": "@alice:elsewhere.org",
                "room_id": "!room:localhost",
                "event_id": "$ev1",
                "content": {
                    "msgtype": "m.text",
                    "body": "first message"
                }
            }]
        });
        let body_str = serde_json::to_string(&body).unwrap();

        let req = Request::builder()
            .method("PUT")
            .uri("/_matrix/app/v1/transactions/txn_dedup?access_token=test_token")
            .header("content-type", "application/json")
            .body(Body::from(body_str.clone()))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(req).await.unwrap().status(),
            StatusCode::OK
        );
        assert!(inbound_rx.try_recv().is_ok());

        let req2 = Request::builder()
            .method("PUT")
            .uri("/_matrix/app/v1/transactions/txn_dedup?access_token=test_token")
            .header("content-type", "application/json")
            .body(Body::from(body_str))
            .unwrap();
        assert_eq!(app.oneshot(req2).await.unwrap().status(), StatusCode::OK);
        assert!(inbound_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_matrix_user_query() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (inbound_tx, _) = mpsc::channel::<InboundMessage>(16);
        let state = make_test_state(inbound_tx);
        state
            .registered_users
            .write()
            .await
            .insert("@octos_agent1:localhost".to_string());

        let app = Router::new()
            .route("/_matrix/app/v1/users/{user_id}", get(handle_user_query))
            .with_state(state);

        let bot_req = Request::builder()
            .method("GET")
            .uri("/_matrix/app/v1/users/@octos_bot:localhost?access_token=test_token")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(bot_req).await.unwrap().status(),
            StatusCode::OK
        );

        let unknown_req = Request::builder()
            .method("GET")
            .uri("/_matrix/app/v1/users/@alice:localhost?access_token=test_token")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(unknown_req).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn test_matrix_send_message() {
        let (homeserver, requests, handle) = spawn_mock_homeserver().await;
        let ch = MatrixChannel::new(
            &homeserver,
            "as_token_test",
            "hs_token_test",
            "localhost",
            "octos_bot",
            "octos_",
            unused_local_port(),
            Arc::new(AtomicBool::new(false)),
        );
        let msg = OutboundMessage {
            channel: "matrix".to_string(),
            chat_id: "!room:localhost".to_string(),
            content: "hello from matrix".to_string(),
            reply_to: None,
            media: vec![],
            metadata: json!({}),
        };

        ch.send(&msg).await.unwrap();

        wait_for_request_count(&requests, 1).await;
        let reqs = requests.lock().await;
        let send_req = reqs
            .iter()
            .find(|r| r.path.contains("/send/"))
            .expect("should have a send request");
        assert_eq!(send_req.method, Method::PUT);
        assert_eq!(send_req.body["body"], "hello from matrix");

        handle.abort();
    }

    #[tokio::test]
    async fn test_matrix_send_with_id() {
        let (homeserver, _requests, handle) = spawn_mock_homeserver().await;
        let ch = MatrixChannel::new(
            &homeserver,
            "as_token_test",
            "hs_token_test",
            "localhost",
            "octos_bot",
            "octos_",
            unused_local_port(),
            Arc::new(AtomicBool::new(false)),
        );
        let msg = OutboundMessage {
            channel: "matrix".to_string(),
            chat_id: "!room:localhost".to_string(),
            content: "hello from matrix".to_string(),
            reply_to: None,
            media: vec![],
            metadata: json!({}),
        };

        let event_id = ch.send_with_id(&msg).await.unwrap();
        assert_eq!(event_id.as_deref(), Some("$test_event"));

        handle.abort();
    }

    #[tokio::test]
    async fn test_matrix_edit_message() {
        let (homeserver, requests, handle) = spawn_mock_homeserver().await;
        let ch = MatrixChannel::new(
            &homeserver,
            "as_token_test",
            "hs_token_test",
            "localhost",
            "octos_bot",
            "octos_",
            unused_local_port(),
            Arc::new(AtomicBool::new(false)),
        );

        ch.edit_message("!room:localhost", "$event1", "**bold** text")
            .await
            .unwrap();

        wait_for_request_count(&requests, 1).await;
        let reqs = requests.lock().await;
        let edit_req = reqs
            .iter()
            .find(|r| r.path.contains("/send/"))
            .expect("should have an edit request");
        assert_eq!(edit_req.body["format"], HTML_FORMAT);
        assert_eq!(edit_req.body["m.relates_to"]["rel_type"], REL_TYPE_REPLACE);
        assert_eq!(edit_req.body["m.relates_to"]["event_id"], "$event1");
        assert!(edit_req.body["formatted_body"].is_string());

        handle.abort();
    }

    #[tokio::test]
    async fn test_matrix_send_typing() {
        let (homeserver, requests, handle) = spawn_mock_homeserver().await;
        let ch = MatrixChannel::new(
            &homeserver,
            "as_token_test",
            "hs_token_test",
            "localhost",
            "octos_bot",
            "octos_",
            unused_local_port(),
            Arc::new(AtomicBool::new(false)),
        );

        ch.send_typing("!room:localhost").await.unwrap();

        wait_for_request_count(&requests, 1).await;
        let reqs = requests.lock().await;
        let typing_req = reqs
            .iter()
            .find(|r| r.path.contains("/typing/"))
            .expect("should have a typing request");
        assert_eq!(typing_req.method, Method::PUT);
        assert_eq!(typing_req.body["typing"], true);
        assert_eq!(typing_req.body["timeout"], 30000);

        handle.abort();
    }

    #[tokio::test]
    async fn test_matrix_send_typing_with_sender_user_id() {
        let (homeserver, requests, handle) = spawn_mock_homeserver().await;
        let ch = MatrixChannel::new(
            &homeserver,
            "as_token_test",
            "hs_token_test",
            "localhost",
            "octos_bot",
            "octos_",
            unused_local_port(),
            Arc::new(AtomicBool::new(false)),
        );

        ch.send_typing_as("!room:localhost", Some("@octos_weather:localhost"))
            .await
            .unwrap();

        wait_for_request_count(&requests, 1).await;
        let reqs = requests.lock().await;
        let typing_req = reqs
            .iter()
            .find(|r| r.path.contains("/typing/"))
            .expect("should have a typing request");
        assert_eq!(typing_req.method, Method::PUT);
        assert!(
            typing_req.path.contains("%40octos_weather%3Alocalhost"),
            "typing path should use sender identity, got: {}",
            typing_req.path
        );
        let query = typing_req.query.as_deref().unwrap_or("");
        assert!(
            query.contains("user_id=%40octos_weather%3Alocalhost"),
            "typing query should use sender identity, got: {query}"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn test_matrix_health_check() {
        let (homeserver, _requests, handle) = spawn_mock_homeserver().await;
        let ch = MatrixChannel::new(
            &homeserver,
            "as_token_test",
            "hs_token_test",
            "localhost",
            "octos_bot",
            "octos_",
            unused_local_port(),
            Arc::new(AtomicBool::new(false)),
        );

        let health = ch.health_check().await.unwrap();
        assert_eq!(health, ChannelHealth::Healthy);

        handle.abort();
    }

    #[tokio::test]
    async fn test_matrix_health_check_down() {
        let (homeserver, _requests, handle) = spawn_mock_homeserver_with_response(
            StatusCode::BAD_GATEWAY,
            json!({"error": "homeserver unavailable"}),
        )
        .await;
        let ch = MatrixChannel::new(
            &homeserver,
            "as_token_test",
            "hs_token_test",
            "localhost",
            "octos_bot",
            "octos_",
            unused_local_port(),
            Arc::new(AtomicBool::new(false)),
        );

        let health = ch.health_check().await.unwrap();
        assert!(matches!(health, ChannelHealth::Down(_)));

        handle.abort();
    }

    #[tokio::test]
    async fn test_matrix_send_html_format() {
        let (homeserver, requests, handle) = spawn_mock_homeserver().await;
        let ch = MatrixChannel::new(
            &homeserver,
            "as_token_test",
            "hs_token_test",
            "localhost",
            "octos_bot",
            "octos_",
            unused_local_port(),
            Arc::new(AtomicBool::new(false)),
        );
        let msg = OutboundMessage {
            channel: "matrix".to_string(),
            chat_id: "!room:localhost".to_string(),
            content: "**bold** text".to_string(),
            reply_to: None,
            media: vec![],
            metadata: json!({}),
        };

        ch.send_with_id(&msg).await.unwrap();

        wait_for_request_count(&requests, 1).await;
        let reqs = requests.lock().await;
        let send_req = reqs
            .iter()
            .find(|r| r.path.contains("/send/"))
            .expect("should have a send request");
        assert_eq!(send_req.body["format"], HTML_FORMAT);
        assert!(send_req.body["formatted_body"].is_string());
        assert_eq!(send_req.body["body"], "**bold** text");

        handle.abort();
    }

    #[tokio::test]
    async fn test_matrix_send_typing_failure_ignored() {
        let (homeserver, _requests, handle) = spawn_mock_homeserver_with_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": "typing failed"}),
        )
        .await;
        let ch = MatrixChannel::new(
            &homeserver,
            "as_token_test",
            "hs_token_test",
            "localhost",
            "octos_bot",
            "octos_",
            unused_local_port(),
            Arc::new(AtomicBool::new(false)),
        );

        ch.send_typing("!room:localhost").await.unwrap();

        handle.abort();
    }

    #[tokio::test]
    async fn test_matrix_send_then_edit_flow() {
        let (homeserver, requests, handle) = spawn_mock_homeserver().await;
        let ch = MatrixChannel::new(
            &homeserver,
            "as_token_test",
            "hs_token_test",
            "localhost",
            "octos_bot",
            "octos_",
            unused_local_port(),
            Arc::new(AtomicBool::new(false)),
        );
        let msg = OutboundMessage {
            channel: "matrix".to_string(),
            chat_id: "!room:localhost".to_string(),
            content: "initial text".to_string(),
            reply_to: None,
            media: vec![],
            metadata: json!({}),
        };

        let event_id = ch.send_with_id(&msg).await.unwrap().unwrap();
        ch.edit_message("!room:localhost", &event_id, "updated text")
            .await
            .unwrap();

        wait_for_request_count(&requests, 2).await;
        let reqs = requests.lock().await;
        let send_req = reqs
            .iter()
            .find(|r| r.path.contains("/send/") && r.body.get("m.relates_to").is_none())
            .expect("should have an initial send request");
        let edit_req = reqs
            .iter()
            .find(|r| r.body.get("m.relates_to").is_some())
            .expect("should have an edit request");
        assert_eq!(send_req.body["body"], "initial text");
        assert_eq!(edit_req.body["m.relates_to"]["event_id"], "$test_event");
        assert_eq!(edit_req.body["m.new_content"]["body"], "updated text");

        handle.abort();
    }

    #[tokio::test]
    async fn test_matrix_event_sender_cache_is_bounded() {
        let (homeserver, _requests, handle) = spawn_mock_homeserver_with_dynamic_event_ids().await;
        let ch = MatrixChannel::new(
            &homeserver,
            "as_token_test",
            "hs_token_test",
            "localhost",
            "octos_bot",
            "octos_",
            unused_local_port(),
            Arc::new(AtomicBool::new(false)),
        );
        let sender = "@octos_weather:localhost";
        ch.registered_users.write().await.insert(sender.to_string());

        let mut first_event_id = None;
        for idx in 0..=MAX_EVENT_SENDER_CACHE {
            let msg = OutboundMessage {
                channel: "matrix".to_string(),
                chat_id: "!room:localhost".to_string(),
                content: format!("message {idx}"),
                reply_to: None,
                media: vec![],
                metadata: json!({ METADATA_SENDER_USER_ID: sender }),
            };
            let event_id = ch.send_with_id(&msg).await.unwrap().unwrap();
            if idx == 0 {
                first_event_id = Some(event_id);
            }
        }

        let senders = ch.event_senders.read().await;
        assert_eq!(senders.len(), MAX_EVENT_SENDER_CACHE);
        assert!(
            !senders
                .iter()
                .any(|(event_id, _)| Some(event_id) == first_event_id.as_ref()),
            "oldest event sender entry should be evicted when cache exceeds the bound"
        );

        handle.abort();
    }

    #[test]
    fn test_reload_error_response_escapes_json() {
        let (status, axum::Json(body)) =
            error_json_response(StatusCode::INTERNAL_SERVER_ERROR, "bad \"quote\"");

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, json!({ "error": "bad \"quote\"" }));
    }

    // ── Registration YAML tests ─────────────────────────────────────────

    #[test]
    fn test_matrix_generate_registration_yaml() {
        let ch = MatrixChannel::new(
            "http://localhost:6167",
            "test-as-token",
            "test-hs-token",
            "localhost",
            "bot",
            "bot_",
            8009,
            Arc::new(AtomicBool::new(false)),
        );

        let tmp = tempfile::tempdir().unwrap();
        let path = ch.generate_registration(tmp.path()).unwrap();

        assert!(path.exists());
        assert_eq!(
            path.file_name().unwrap(),
            "matrix-appservice-registration.yaml"
        );

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("id:"), "missing id field");
        assert!(
            content.contains("as_token: test-as-token"),
            "missing as_token"
        );
        assert!(
            content.contains("hs_token: test-hs-token"),
            "missing hs_token"
        );
        assert!(
            content.contains("sender_localpart: bot"),
            "missing sender_localpart"
        );
        assert!(
            content.contains("url: http://localhost:8009"),
            "missing url"
        );
        assert!(
            content.contains("@bot_.*:localhost"),
            "missing user namespace regex"
        );
        assert!(content.contains("namespaces:"), "missing namespaces");
    }

    #[test]
    fn test_matrix_registration_no_overwrite() {
        let ch = make_channel();
        let tmp = tempfile::tempdir().unwrap();
        let file_path = tmp.path().join("matrix-appservice-registration.yaml");

        // Write a custom file first
        std::fs::write(&file_path, "custom").unwrap();

        let returned_path = ch.generate_registration(tmp.path()).unwrap();
        assert_eq!(returned_path, file_path);

        let content = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "custom", "existing file should not be overwritten");
    }

    #[test]
    fn test_matrix_registration_parseable() {
        let ch = make_channel();
        let tmp = tempfile::tempdir().unwrap();
        let path = ch.generate_registration(tmp.path()).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_yml::from_str(&content).unwrap();

        assert!(
            parsed.get("as_token").is_some(),
            "parsed YAML missing as_token"
        );
        assert!(
            parsed.get("hs_token").is_some(),
            "parsed YAML missing hs_token"
        );
        assert!(
            parsed.get("sender_localpart").is_some(),
            "parsed YAML missing sender_localpart"
        );
        assert!(
            parsed.get("namespaces").is_some(),
            "parsed YAML missing namespaces"
        );
    }

    // ── BotRouter tests ─────────────────────────────────────────────────

    #[tokio::test]
    async fn test_bot_router_register_and_route() {
        let router = BotRouter::new(None);
        router
            .register("@bot_weather:localhost", "profile-weather-001")
            .await
            .unwrap();
        let result = router.route("@bot_weather:localhost").await;
        assert_eq!(result, Some("profile-weather-001".to_string()));
    }

    #[tokio::test]
    async fn test_bot_router_unknown_returns_none() {
        let router = BotRouter::new(None);
        let result = router.route("@bot_unknown:localhost").await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_bot_router_unregister() {
        let router = BotRouter::new(None);
        router
            .register("@bot_weather:localhost", "profile-001")
            .await
            .unwrap();
        router.unregister("@bot_weather:localhost").await.unwrap();
        let result = router.route("@bot_weather:localhost").await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_bot_router_persistence() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("matrix-bot-routes.json");

        // Register a mapping and persist
        {
            let router = BotRouter::new(Some(path.clone()));
            router
                .register("@bot_a:localhost", "profile-a")
                .await
                .unwrap();
        }

        // Create a new router from the same path and verify it loaded
        let router2 = BotRouter::new(Some(path));
        let result = router2.route("@bot_a:localhost").await;
        assert_eq!(result, Some("profile-a".to_string()));
    }

    #[test]
    fn test_bot_visibility_serializes_lowercase() {
        let public_json = serde_json::to_string(&BotVisibility::Public).unwrap();
        assert_eq!(public_json, "\"public\"");

        let private_json = serde_json::to_string(&BotVisibility::Private).unwrap();
        assert_eq!(private_json, "\"private\"");
    }

    #[tokio::test]
    async fn test_bot_router_loads_old_format() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("matrix-bot-routes.json");
        std::fs::write(&path, r#"{"@bot_weather:localhost":"main--weather"}"#).unwrap();

        let router = BotRouter::new(Some(path));
        let entry = router
            .get_entry("@bot_weather:localhost")
            .await
            .expect("legacy route should load");

        assert_eq!(entry.profile_id, "main--weather");
        assert_eq!(entry.owner, "");
        assert_eq!(entry.visibility, BotVisibility::Public);
    }

    #[tokio::test]
    async fn test_bot_router_loads_new_format() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("matrix-bot-routes.json");
        std::fs::write(
            &path,
            r#"{"@bot_weather:localhost":{"profile_id":"main--weather","owner":"@alice:localhost","visibility":"public"}}"#,
        )
        .unwrap();

        let router = BotRouter::new(Some(path));
        let entry = router
            .get_entry("@bot_weather:localhost")
            .await
            .expect("new-format route should load");

        assert_eq!(entry.profile_id, "main--weather");
        assert_eq!(entry.owner, "@alice:localhost");
        assert_eq!(entry.visibility, BotVisibility::Public);
    }

    #[tokio::test]
    async fn test_matrix_register_bot_registers_user_and_route() {
        let (homeserver, requests, homeserver_handle) = spawn_mock_homeserver().await;
        let ch = MatrixChannel::new(
            &homeserver,
            "as_token_test",
            "hs_token_test",
            "localhost",
            "octos_bot",
            "octos_",
            9880,
            Arc::new(AtomicBool::new(false)),
        );

        ch.register_bot("@octos_weather:localhost", "profile-weather")
            .await
            .unwrap();

        assert_eq!(
            ch.bot_router().route("@octos_weather:localhost").await,
            Some("profile-weather".to_string())
        );

        wait_for_request_count(&requests, 1).await;
        let requests = requests.lock().await;
        assert!(
            requests
                .iter()
                .any(|req| req.path == "/_matrix/client/v3/register")
        );

        homeserver_handle.abort();
    }

    #[tokio::test]
    async fn test_matrix_unregister_bot_removes_route() {
        let ch = make_channel();
        ch.bot_router()
            .register("@octos_weather:localhost", "profile-weather")
            .await
            .unwrap();

        ch.unregister_bot("@octos_weather:localhost").await.unwrap();

        assert_eq!(
            ch.bot_router().route("@octos_weather:localhost").await,
            None
        );
    }

    #[tokio::test]
    async fn test_matrix_unregister_bot_removes_registered_sender() {
        let ch = make_channel();
        {
            let mut users = ch.registered_users.write().await;
            users.insert("@octos_weather:localhost".to_string());
        }
        ch.bot_router()
            .register("@octos_weather:localhost", "profile-weather")
            .await
            .unwrap();

        ch.unregister_bot("@octos_weather:localhost").await.unwrap();

        let users = ch.registered_users.read().await;
        assert!(
            !users.contains("@octos_weather:localhost"),
            "unregister_bot should remove sender authorization"
        );
    }

    #[tokio::test]
    async fn test_matrix_register_bot_fails_when_route_persist_fails() {
        let (homeserver, _requests, homeserver_handle) = spawn_mock_homeserver().await;
        let tmp = tempfile::tempdir().unwrap();
        let missing_data_dir = tmp.path().join("missing-dir");
        let ch = MatrixChannel::new(
            &homeserver,
            "as_token_test",
            "hs_token_test",
            "localhost",
            "octos_bot",
            "octos_",
            9880,
            Arc::new(AtomicBool::new(false)),
        )
        .with_bot_router(&missing_data_dir);

        let result = ch
            .register_bot("@octos_weather:localhost", "profile-weather")
            .await;
        assert!(
            result.is_err(),
            "register_bot should fail when route persistence fails"
        );
        assert_eq!(
            ch.bot_router().route("@octos_weather:localhost").await,
            None,
            "failed registration should not leave an in-memory route"
        );
        let users = ch.registered_users.read().await;
        assert!(
            !users.contains("@octos_weather:localhost"),
            "failed registration should not leave sender authorization"
        );

        homeserver_handle.abort();
    }

    /// Regression test: after gateway restart, bots persisted in the route map
    /// must be restored into `registered_users` so outbound sends succeed.
    #[tokio::test]
    async fn test_startup_restores_registered_users_from_persisted_routes() {
        let (homeserver, requests, handle) = spawn_mock_homeserver().await;
        let tmp = tempfile::tempdir().unwrap();

        // Pre-populate a routes file (simulating a prior gateway session)
        let routes_path = tmp.path().join("matrix-bot-routes.json");
        std::fs::write(
            &routes_path,
            r#"{"@octos_weather:localhost":"profile-weather"}"#,
        )
        .unwrap();

        let ch = MatrixChannel::new(
            &homeserver,
            "as_token_test",
            "hs_token_test",
            "localhost",
            "octos_bot",
            "octos_",
            unused_local_port(),
            Arc::new(AtomicBool::new(false)),
        )
        .with_bot_router(tmp.path());

        // Before start: registered_users should be empty
        assert!(
            !ch.registered_users
                .read()
                .await
                .contains("@octos_weather:localhost"),
            "bot should not be in registered_users before start"
        );

        // Simulate what start() does: register bot user + restore from routes
        {
            let mut users = ch.registered_users.write().await;
            users.insert(ch.bot_user_id.clone());
            for (matrix_user_id, _) in ch.bot_router.list_routes().await {
                users.insert(matrix_user_id);
            }
        }

        // After restore: the persisted bot should be in registered_users
        assert!(
            ch.registered_users
                .read()
                .await
                .contains("@octos_weather:localhost"),
            "persisted bot must be restored into registered_users on startup"
        );

        // Outbound send should succeed (not rejected as unregistered)
        let msg = OutboundMessage {
            channel: "matrix".to_string(),
            chat_id: "!room:localhost".to_string(),
            content: "Hello from restored bot".to_string(),
            reply_to: None,
            media: vec![],
            metadata: json!({"sender_user_id": "@octos_weather:localhost"}),
        };
        ch.send_with_id(&msg)
            .await
            .expect("send should succeed for restored bot");

        wait_for_request_count(&requests, 1).await;
        let reqs = requests.lock().await;
        let send_req = reqs.iter().find(|r| r.path.contains("/send/")).unwrap();
        let query = send_req.query.as_deref().unwrap_or("");
        assert!(
            query.contains("user_id=%40octos_weather%3Alocalhost"),
            "send should use the restored bot identity, got query: {query}"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn test_bot_router_no_metadata_without_mapping() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);

        // State with an empty bot router (no mappings)
        let state = make_test_state(inbound_tx);

        let app = Router::new()
            .route(
                "/_matrix/app/v1/transactions/{txn_id}",
                put(handle_transaction),
            )
            .with_state(state);

        let body = json!({
            "events": [{
                "type": "m.room.message",
                "sender": "@alice:elsewhere.org",
                "room_id": "!room123:localhost",
                "event_id": "$ev_no_route",
                "content": {
                    "msgtype": "m.text",
                    "body": "hello, no bot mentioned here"
                }
            }]
        });

        let req = Request::builder()
            .method("PUT")
            .uri("/_matrix/app/v1/transactions/txn_no_route?access_token=test_token")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let msg = inbound_rx.try_recv().unwrap();
        assert!(
            msg.metadata.get(METADATA_TARGET_PROFILE_ID).is_none(),
            "metadata should not contain target_profile_id when no bot mapping exists"
        );
    }

    #[tokio::test]
    async fn test_bot_router_injects_metadata() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);

        let mut state = make_test_state(inbound_tx);

        // Register a bot mapping in the router
        let router = BotRouter::new(None);
        router
            .register("@bot_weather:localhost", "profile-weather")
            .await
            .unwrap();
        state.bot_router = Arc::new(router);

        let app = Router::new()
            .route(
                "/_matrix/app/v1/transactions/{txn_id}",
                put(handle_transaction),
            )
            .with_state(state);

        let body = json!({
            "events": [{
                "type": "m.room.message",
                "sender": "@alice:elsewhere.org",
                "room_id": "!room123:localhost",
                "event_id": "$ev_routed",
                "content": {
                    "msgtype": "m.text",
                    "body": "Hey @bot_weather:localhost what is the forecast?"
                }
            }]
        });

        let req = Request::builder()
            .method("PUT")
            .uri("/_matrix/app/v1/transactions/txn_routed?access_token=test_token")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let msg = inbound_rx.try_recv().unwrap();
        assert_eq!(
            msg.metadata
                .get(METADATA_TARGET_PROFILE_ID)
                .and_then(|v| v.as_str()),
            Some("profile-weather"),
            "metadata should contain the routed profile_id"
        );
    }

    #[tokio::test]
    async fn test_bot_router_does_not_match_user_id_substrings() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);

        let mut state = make_test_state(inbound_tx);
        let router = BotRouter::new(None);
        router
            .register("@bot_weather:localhost", "profile-weather")
            .await
            .unwrap();
        state.bot_router = Arc::new(router);

        let app = Router::new()
            .route(
                "/_matrix/app/v1/transactions/{txn_id}",
                put(handle_transaction),
            )
            .with_state(state);

        let body = json!({
            "events": [{
                "type": "m.room.message",
                "sender": "@alice:elsewhere.org",
                "room_id": "!room123:localhost",
                "event_id": "$ev_substring",
                "content": {
                    "msgtype": "m.text",
                    "body": "Hey @bot_weather:localhost123 are you there?"
                }
            }]
        });

        let req = Request::builder()
            .method("PUT")
            .uri("/_matrix/app/v1/transactions/txn_substring?access_token=test_token")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let msg = inbound_rx.try_recv().unwrap();
        assert!(
            msg.metadata.get(METADATA_TARGET_PROFILE_ID).is_none(),
            "substring matches should not route to a bot profile"
        );
    }

    // ── Track A: sender_user_id tests ──────────────────────────────────────

    #[tokio::test]
    async fn test_matrix_send_with_sender_user_id() {
        let (homeserver, requests, handle) = spawn_mock_homeserver().await;
        let ch = MatrixChannel::new(
            &homeserver,
            "as_token_test",
            "hs_token_test",
            "localhost",
            "octos_bot",
            "octos_",
            unused_local_port(),
            Arc::new(AtomicBool::new(false)),
        );
        // Register the virtual user so it's allowed
        ch.registered_users
            .write()
            .await
            .insert("@octos_weather:localhost".to_string());

        let msg = OutboundMessage {
            channel: "matrix".to_string(),
            chat_id: "!room:localhost".to_string(),
            content: "Hello from weather bot".to_string(),
            reply_to: None,
            media: vec![],
            metadata: json!({"sender_user_id": "@octos_weather:localhost"}),
        };

        ch.send_with_id(&msg).await.unwrap();

        wait_for_request_count(&requests, 1).await;
        let reqs = requests.lock().await;
        let send_req = reqs
            .iter()
            .find(|r| r.path.contains("/send/"))
            .expect("should have a send request");
        let query = send_req.query.as_deref().unwrap_or("");
        assert!(
            query.contains("user_id=%40octos_weather%3Alocalhost"),
            "URL should use sender_user_id from metadata, got query: {query}"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn test_matrix_send_default_sender() {
        let (homeserver, requests, handle) = spawn_mock_homeserver().await;
        let ch = MatrixChannel::new(
            &homeserver,
            "as_token_test",
            "hs_token_test",
            "localhost",
            "octos_bot",
            "octos_",
            unused_local_port(),
            Arc::new(AtomicBool::new(false)),
        );

        // No sender_user_id in metadata → should use default bot_user_id
        let msg = OutboundMessage {
            channel: "matrix".to_string(),
            chat_id: "!room:localhost".to_string(),
            content: "Hello from default bot".to_string(),
            reply_to: None,
            media: vec![],
            metadata: json!({}),
        };

        ch.send_with_id(&msg).await.unwrap();

        wait_for_request_count(&requests, 1).await;
        let reqs = requests.lock().await;
        let send_req = reqs
            .iter()
            .find(|r| r.path.contains("/send/"))
            .expect("should have a send request");
        let query = send_req.query.as_deref().unwrap_or("");
        assert!(
            query.contains("user_id=%40octos_bot%3Alocalhost"),
            "URL should use default bot_user_id when sender_user_id is absent, got query: {query}"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn test_matrix_send_rejects_unregistered_sender() {
        let (homeserver, _requests, handle) = spawn_mock_homeserver().await;
        let ch = MatrixChannel::new(
            &homeserver,
            "as_token_test",
            "hs_token_test",
            "localhost",
            "octos_bot",
            "octos_",
            unused_local_port(),
            Arc::new(AtomicBool::new(false)),
        );
        // Do NOT register @octos_unknown:localhost

        let msg = OutboundMessage {
            channel: "matrix".to_string(),
            chat_id: "!room:localhost".to_string(),
            content: "Hello from unknown bot".to_string(),
            reply_to: None,
            media: vec![],
            metadata: json!({"sender_user_id": "@octos_unknown:localhost"}),
        };

        let result = ch.send_with_id(&msg).await;
        assert!(result.is_err(), "should reject unregistered sender_user_id");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("not registered"),
            "error should mention 'not registered', got: {err_msg}"
        );

        handle.abort();
    }

    // ── DM routing (room-bot mapping) tests ─────────────────────────

    #[tokio::test]
    async fn test_bot_router_add_room_bot_and_route_by_room() {
        let router = BotRouter::new(None);
        router
            .add_room_bot("!dm1:localhost", "profile-weather")
            .await
            .unwrap();

        let result = router.route_by_room("!dm1:localhost").await;
        assert_eq!(result, Some("profile-weather".to_string()));
    }

    #[tokio::test]
    async fn test_bot_router_route_by_room_multi_bot_returns_none() {
        let router = BotRouter::new(None);
        router
            .add_room_bot("!group:localhost", "profile-weather")
            .await
            .unwrap();
        router
            .add_room_bot("!group:localhost", "profile-news")
            .await
            .unwrap();

        // Multiple bots in room → require @mention
        let result = router.route_by_room("!group:localhost").await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_bot_router_route_by_room_unknown_room() {
        let router = BotRouter::new(None);
        let result = router.route_by_room("!unknown:localhost").await;
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn test_bot_router_remove_bot_from_rooms() {
        let router = BotRouter::new(None);
        router
            .add_room_bot("!dm1:localhost", "profile-weather")
            .await
            .unwrap();
        router
            .add_room_bot("!dm2:localhost", "profile-weather")
            .await
            .unwrap();
        router
            .add_room_bot("!dm2:localhost", "profile-news")
            .await
            .unwrap();

        router
            .remove_bot_from_rooms("profile-weather")
            .await
            .unwrap();

        assert_eq!(router.route_by_room("!dm1:localhost").await, None);
        // dm2 still has profile-news
        assert_eq!(
            router.route_by_room("!dm2:localhost").await,
            Some("profile-news".to_string())
        );
    }

    #[tokio::test]
    async fn test_bot_router_room_map_persistence() {
        let tmp = tempfile::tempdir().unwrap();
        let routes_path = tmp.path().join("matrix-bot-routes.json");

        // Create router, add room mapping
        {
            let router = BotRouter::new(Some(routes_path.clone()));
            router
                .add_room_bot("!dm1:localhost", "profile-weather")
                .await
                .unwrap();
        }

        // New router from same path should load room mappings
        let router2 = BotRouter::new(Some(routes_path));
        assert_eq!(
            router2.route_by_room("!dm1:localhost").await,
            Some("profile-weather".to_string())
        );
    }

    #[tokio::test]
    async fn test_unregister_bot_cleans_room_mappings() {
        let ch = make_channel();
        ch.bot_router()
            .register("@octos_weather:localhost", "profile-weather")
            .await
            .unwrap();
        ch.bot_router()
            .add_room_bot("!dm1:localhost", "profile-weather")
            .await
            .unwrap();

        // Add to registered_users so unregister_bot can clean up
        ch.registered_users
            .write()
            .await
            .insert("@octos_weather:localhost".to_string());

        ch.unregister_bot("@octos_weather:localhost").await.unwrap();

        // Room mapping should be cleaned up
        assert_eq!(ch.bot_router().route_by_room("!dm1:localhost").await, None);
        // User route should also be gone
        assert_eq!(
            ch.bot_router().route("@octos_weather:localhost").await,
            None
        );
    }

    #[tokio::test]
    async fn test_handle_transaction_dm_routing() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
        let mut state = make_test_state(inbound_tx);

        // Set up a room-bot mapping (simulate bot already joined DM room)
        let router = BotRouter::new(None);
        router
            .register("@bot_weather:localhost", "profile-weather")
            .await
            .unwrap();
        router
            .add_room_bot("!dm_room:localhost", "profile-weather")
            .await
            .unwrap();
        state.bot_router = Arc::new(router);

        let app = Router::new()
            .route(
                "/_matrix/app/v1/transactions/{txn_id}",
                put(handle_transaction),
            )
            .with_state(state);

        // Send a message WITHOUT @mention in the DM room
        let body = serde_json::json!({
            "events": [{
                "type": "m.room.message",
                "sender": "@alice:localhost",
                "room_id": "!dm_room:localhost",
                "event_id": "$dm1",
                "content": {
                    "msgtype": "m.text",
                    "body": "What's the weather today?"
                }
            }]
        });

        let req = Request::builder()
            .method("PUT")
            .uri("/_matrix/app/v1/transactions/txn-dm-1?access_token=test_token")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let msg = inbound_rx.try_recv().unwrap();
        assert_eq!(
            msg.metadata
                .get(METADATA_TARGET_PROFILE_ID)
                .and_then(|v| v.as_str()),
            Some("profile-weather"),
            "DM message should route to weather bot via room mapping"
        );
    }

    #[tokio::test]
    async fn test_private_bot_message_blocked_for_non_owner() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (homeserver, requests, homeserver_handle) = spawn_mock_homeserver().await;
        let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
        let mut state = make_test_state(inbound_tx);
        state.homeserver = homeserver;

        let router = BotRouter::new(None);
        router
            .register_entry(
                "@octos_private:localhost",
                "main--private",
                "@owner:localhost",
                BotVisibility::Private,
            )
            .await
            .unwrap();
        router
            .add_room_bot("!private:localhost", "main--private")
            .await
            .unwrap();
        state.bot_router = Arc::new(router);

        let app = Router::new()
            .route(
                "/_matrix/app/v1/transactions/{txn_id}",
                put(handle_transaction),
            )
            .with_state(state);

        let body = json!({
            "events": [{
                "type": "m.room.message",
                "sender": "@mallory:localhost",
                "room_id": "!private:localhost",
                "event_id": "$private1",
                "content": {
                    "msgtype": "m.text",
                    "body": "hello private bot"
                }
            }]
        });

        let req = Request::builder()
            .method("PUT")
            .uri("/_matrix/app/v1/transactions/txn-private-msg?access_token=test_token")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            inbound_rx.try_recv().is_err(),
            "non-owner message should not be forwarded to the agent"
        );

        wait_for_request_count(&requests, 1).await;
        let requests = requests.lock().await;
        assert!(requests.iter().any(|req| {
            req.path.contains("/send/")
                && req
                    .query
                    .as_deref()
                    .is_some_and(|q| q.contains("user_id=%40octos_private%3Alocalhost"))
        }));

        homeserver_handle.abort();
    }

    #[tokio::test]
    async fn test_private_bot_message_allowed_for_owner() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
        let mut state = make_test_state(inbound_tx);

        let router = BotRouter::new(None);
        router
            .register_entry(
                "@octos_private:localhost",
                "main--private",
                "@owner:localhost",
                BotVisibility::Private,
            )
            .await
            .unwrap();
        router
            .add_room_bot("!private:localhost", "main--private")
            .await
            .unwrap();
        state.bot_router = Arc::new(router);

        let app = Router::new()
            .route(
                "/_matrix/app/v1/transactions/{txn_id}",
                put(handle_transaction),
            )
            .with_state(state);

        let body = json!({
            "events": [{
                "type": "m.room.message",
                "sender": "@owner:localhost",
                "room_id": "!private:localhost",
                "event_id": "$private-owner",
                "content": {
                    "msgtype": "m.text",
                    "body": "hello private bot"
                }
            }]
        });

        let req = Request::builder()
            .method("PUT")
            .uri("/_matrix/app/v1/transactions/txn-private-owner?access_token=test_token")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let msg = inbound_rx.try_recv().expect("owner message should be forwarded");
        assert_eq!(
            msg.metadata
                .get(METADATA_TARGET_PROFILE_ID)
                .and_then(|v| v.as_str()),
            Some("main--private")
        );
    }

    #[tokio::test]
    async fn test_handle_transaction_mention_priority() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
        let mut state = make_test_state(inbound_tx);

        let router = BotRouter::new(None);
        router
            .register("@bot_weather:localhost", "profile-weather")
            .await
            .unwrap();
        router
            .register("@bot_news:localhost", "profile-news")
            .await
            .unwrap();
        // Room is mapped to weather bot
        router
            .add_room_bot("!room:localhost", "profile-weather")
            .await
            .unwrap();
        state.bot_router = Arc::new(router);

        let app = Router::new()
            .route(
                "/_matrix/app/v1/transactions/{txn_id}",
                put(handle_transaction),
            )
            .with_state(state);

        // Message mentions news bot, even though room is mapped to weather
        let body = serde_json::json!({
            "events": [{
                "type": "m.room.message",
                "sender": "@alice:localhost",
                "room_id": "!room:localhost",
                "event_id": "$mention1",
                "content": {
                    "msgtype": "m.text",
                    "body": "@bot_news:localhost what's the latest?"
                }
            }]
        });

        let req = Request::builder()
            .method("PUT")
            .uri("/_matrix/app/v1/transactions/txn-mention-1?access_token=test_token")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let msg = inbound_rx.try_recv().unwrap();
        assert_eq!(
            msg.metadata
                .get(METADATA_TARGET_PROFILE_ID)
                .and_then(|v| v.as_str()),
            Some("profile-news"),
            "@mention should take priority over room mapping"
        );
    }

    #[tokio::test]
    async fn test_handle_transaction_m_mentions_routes_to_target_bot() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
        let mut state = make_test_state(inbound_tx);

        let router = BotRouter::new(None);
        router
            .register("@octos_mybot:localhost", "profile-mybot")
            .await
            .unwrap();
        state.bot_router = Arc::new(router);

        let app = Router::new()
            .route(
                "/_matrix/app/v1/transactions/{txn_id}",
                put(handle_transaction),
            )
            .with_state(state);

        let body = serde_json::json!({
            "events": [{
                "type": "m.room.message",
                "sender": "@alice:localhost",
                "room_id": "!room:localhost",
                "event_id": "$mentions1",
                "content": {
                    "msgtype": "m.text",
                    "body": "mybot: 你又是谁",
                    "m.mentions": {
                        "user_ids": ["@octos_mybot:localhost"]
                    }
                }
            }]
        });

        let req = Request::builder()
            .method("PUT")
            .uri("/_matrix/app/v1/transactions/txn-mentions-1?access_token=test_token")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let msg = inbound_rx.try_recv().unwrap();
        assert_eq!(
            msg.metadata
                .get(METADATA_TARGET_PROFILE_ID)
                .and_then(|v| v.as_str()),
            Some("profile-mybot"),
            "m.mentions user_ids should route to the selected bot"
        );
    }

    #[tokio::test]
    async fn test_handle_transaction_explicit_target_user_id() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
        let mut state = make_test_state(inbound_tx);

        let router = BotRouter::new(None);
        router
            .register("@bot_weather:localhost", "profile-weather")
            .await
            .unwrap();
        state.bot_router = Arc::new(router);

        let app = Router::new()
            .route(
                "/_matrix/app/v1/transactions/{txn_id}",
                put(handle_transaction),
            )
            .with_state(state);

        let body = serde_json::json!({
            "events": [{
                "type": "m.room.message",
                "sender": "@alice:localhost",
                "room_id": "!room:localhost",
                "event_id": "$explicit1",
                "content": {
                    "msgtype": "m.text",
                    "body": "What's the weather today?",
                    "org.octos.target_user_id": "@bot_weather:localhost"
                }
            }]
        });

        let req = Request::builder()
            .method("PUT")
            .uri("/_matrix/app/v1/transactions/txn-explicit-1?access_token=test_token")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let msg = inbound_rx.try_recv().unwrap();
        assert_eq!(
            msg.metadata
                .get(METADATA_TARGET_PROFILE_ID)
                .and_then(|v| v.as_str()),
            Some("profile-weather"),
            "explicit target_user_id should route to the selected bot"
        );
    }

    #[tokio::test]
    async fn test_handle_transaction_explicit_target_user_id_priority() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let (inbound_tx, mut inbound_rx) = mpsc::channel::<InboundMessage>(16);
        let mut state = make_test_state(inbound_tx);

        let router = BotRouter::new(None);
        router
            .register("@bot_weather:localhost", "profile-weather")
            .await
            .unwrap();
        router
            .register("@bot_news:localhost", "profile-news")
            .await
            .unwrap();
        router
            .add_room_bot("!room:localhost", "profile-weather")
            .await
            .unwrap();
        state.bot_router = Arc::new(router);

        let app = Router::new()
            .route(
                "/_matrix/app/v1/transactions/{txn_id}",
                put(handle_transaction),
            )
            .with_state(state);

        let body = serde_json::json!({
            "events": [{
                "type": "m.room.message",
                "sender": "@alice:localhost",
                "room_id": "!room:localhost",
                "event_id": "$explicit2",
                "content": {
                    "msgtype": "m.text",
                    "body": "@bot_news:localhost what's the weather today?",
                    "org.octos.target_user_id": "@bot_weather:localhost"
                }
            }]
        });

        let req = Request::builder()
            .method("PUT")
            .uri("/_matrix/app/v1/transactions/txn-explicit-2?access_token=test_token")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let msg = inbound_rx.try_recv().unwrap();
        assert_eq!(
            msg.metadata
                .get(METADATA_TARGET_PROFILE_ID)
                .and_then(|v| v.as_str()),
            Some("profile-weather"),
            "explicit target_user_id should take priority over mention and room routing"
        );
    }

    // ── Slash command parsing tests ──────────────────────────────────

    #[test]
    fn test_extract_prompt_flag_no_prompt() {
        let (args, prompt) = extract_prompt_flag("weather Weather Bot");
        assert_eq!(args, "weather Weather Bot");
        assert!(prompt.is_none());
    }

    #[test]
    fn test_extract_prompt_flag_quoted() {
        let (args, prompt) = extract_prompt_flag("weather Weather Bot --prompt \"你是天气助手\"");
        assert_eq!(args, "weather Weather Bot");
        assert_eq!(prompt.as_deref(), Some("你是天气助手"));
    }

    #[test]
    fn test_extract_prompt_flag_unquoted() {
        let (args, prompt) = extract_prompt_flag("weather --prompt simple prompt text");
        assert_eq!(args, "weather");
        assert_eq!(prompt.as_deref(), Some("simple prompt text"));
    }

    #[test]
    fn test_extract_prompt_flag_empty_prompt() {
        let (args, prompt) = extract_prompt_flag("weather --prompt");
        assert_eq!(args, "weather");
        assert!(prompt.is_none());
    }

    #[test]
    fn test_extract_visibility_flag_public() {
        let (args, visibility) =
            extract_visibility_flag("weather Weather Bot --public --prompt \"hello\"");
        assert_eq!(args, "weather Weather Bot --prompt \"hello\"");
        assert_eq!(visibility, Some(BotVisibility::Public));
    }

    #[test]
    fn test_extract_visibility_flag_private() {
        let (args, visibility) = extract_visibility_flag("weather Weather Bot --private");
        assert_eq!(args, "weather Weather Bot");
        assert_eq!(visibility, Some(BotVisibility::Private));
    }

    #[test]
    fn test_extract_visibility_flag_missing() {
        let (args, visibility) = extract_visibility_flag("weather Weather Bot");
        assert_eq!(args, "weather Weather Bot");
        assert_eq!(visibility, None);
    }

    #[tokio::test]
    async fn test_slash_command_not_intercepted_without_bot_manager() {
        let (tx, _rx) = mpsc::channel(1);
        let state = make_test_state(tx);
        // bot_manager is None, so slash commands should not be intercepted
        let result =
            handle_slash_command(&state, "@alice:localhost", "!room:localhost", "/listbots").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_slash_command_not_intercepted_for_normal_messages() {
        let (tx, _rx) = mpsc::channel(1);
        let mut state = make_test_state(tx);
        state.bot_manager = Some(Arc::new(MockBotManager));

        let result =
            handle_slash_command(&state, "@alice:localhost", "!room:localhost", "hello world")
                .await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_slash_command_listbots() {
        let (tx, _rx) = mpsc::channel(1);
        let mut state = make_test_state(tx);
        state.bot_manager = Some(Arc::new(MockBotManager));

        let result =
            handle_slash_command(&state, "@alice:localhost", "!room:localhost", "/listbots").await;
        assert!(result.is_some());
        assert!(result.unwrap().contains("mock list"));
    }

    #[tokio::test]
    async fn test_slash_command_createbot() {
        let (tx, _rx) = mpsc::channel(1);
        let mut state = make_test_state(tx);
        state.bot_manager = Some(Arc::new(MockBotManager));

        let result = handle_slash_command(
            &state,
            "@alice:localhost",
            "!room:localhost",
            "/createbot weather Weather Bot --prompt \"你是天气助手\"",
        )
        .await;
        assert!(result.is_some());
        let msg = result.unwrap();
        assert!(msg.contains("mock create"), "got: {msg}");
    }

    #[tokio::test]
    async fn test_slash_command_createbot_defaults_private() {
        let (tx, _rx) = mpsc::channel(1);
        let mut state = make_test_state(tx);
        state.bot_manager = Some(Arc::new(RecordingBotManager::default()));

        let result = handle_slash_command(
            &state,
            "@alice:localhost",
            "!room:localhost",
            "/createbot weather Weather Bot",
        )
        .await;

        let msg = result.expect("createbot should be intercepted");
        assert!(msg.contains("mock create"), "got: {msg}");
        assert!(msg.contains("Private"), "expected default private visibility: {msg}");
    }

    #[tokio::test]
    async fn test_slash_command_createbot_with_public_visibility() {
        let (tx, _rx) = mpsc::channel(1);
        let mut state = make_test_state(tx);
        state.bot_manager = Some(Arc::new(RecordingBotManager::default()));

        let result = handle_slash_command(
            &state,
            "@alice:localhost",
            "!room:localhost",
            "/createbot weather Weather Bot --public",
        )
        .await;

        let msg = result.expect("createbot should be intercepted");
        assert!(msg.contains("mock create"), "got: {msg}");
        assert!(msg.contains("Public"), "expected public visibility: {msg}");
    }

    #[tokio::test]
    async fn test_slash_command_deletebot() {
        let (tx, _rx) = mpsc::channel(1);
        let mut state = make_test_state(tx);
        state.bot_manager = Some(Arc::new(MockBotManager));

        let result = handle_slash_command(
            &state,
            "@alice:localhost",
            "!room:localhost",
            "/deletebot @bot_weather:localhost",
        )
        .await;
        assert!(result.is_some());
        assert!(result.unwrap().contains("mock delete"));
    }

    #[tokio::test]
    async fn test_slash_command_createbot_missing_args() {
        let (tx, _rx) = mpsc::channel(1);
        let mut state = make_test_state(tx);
        state.bot_manager = Some(Arc::new(MockBotManager));

        let result =
            handle_slash_command(&state, "@alice:localhost", "!room:localhost", "/createbot").await;
        assert!(result.is_some());
        assert!(result.unwrap().contains("Usage"));
    }

    #[tokio::test]
    async fn test_slash_command_deletebot_missing_args() {
        let (tx, _rx) = mpsc::channel(1);
        let mut state = make_test_state(tx);
        state.bot_manager = Some(Arc::new(MockBotManager));

        let result =
            handle_slash_command(&state, "@alice:localhost", "!room:localhost", "/deletebot").await;
        assert!(result.is_some());
        assert!(result.unwrap().contains("Usage"));
    }

    #[tokio::test]
    async fn test_slash_command_listbot_singular_alias() {
        let (tx, _rx) = mpsc::channel(1);
        let mut state = make_test_state(tx);
        state.bot_manager = Some(Arc::new(MockBotManager));

        let result =
            handle_slash_command(&state, "@alice:localhost", "!room:localhost", "/listbot").await;
        assert!(result.is_some());
        assert!(result.unwrap().contains("mock list"));
    }

    #[tokio::test]
    async fn test_slash_command_unknown_command_not_intercepted() {
        let (tx, _rx) = mpsc::channel(1);
        let mut state = make_test_state(tx);
        state.bot_manager = Some(Arc::new(MockBotManager));

        let result =
            handle_slash_command(&state, "@alice:localhost", "!room:localhost", "/unknown").await;
        assert!(
            result.is_none(),
            "unknown slash commands should pass through to agent"
        );
    }

    /// Mock BotManager for testing slash command dispatch.
    struct MockBotManager;

    #[derive(Default)]
    struct RecordingBotManager;

    #[async_trait]
    impl BotManager for MockBotManager {
        async fn create_bot(
            &self,
            username: &str,
            name: &str,
            _system_prompt: Option<&str>,
            _sender: &str,
            visibility: BotVisibility,
        ) -> Result<String> {
            Ok(format!("mock create: {username} ({name}) {visibility:?}"))
        }
        async fn delete_bot(&self, matrix_user_id: &str, _sender: &str) -> Result<String> {
            Ok(format!("mock delete: {matrix_user_id}"))
        }
        async fn list_bots(&self, _sender: &str) -> Result<String> {
            Ok("mock list: no bots".to_string())
        }
    }

    #[async_trait]
    impl BotManager for RecordingBotManager {
        async fn create_bot(
            &self,
            username: &str,
            name: &str,
            _system_prompt: Option<&str>,
            _sender: &str,
            visibility: BotVisibility,
        ) -> Result<String> {
            Ok(format!("mock create: {username} ({name}) {visibility:?}"))
        }

        async fn delete_bot(&self, matrix_user_id: &str, _sender: &str) -> Result<String> {
            Ok(format!("mock delete: {matrix_user_id}"))
        }

        async fn list_bots(&self, _sender: &str) -> Result<String> {
            Ok("mock list: no bots".to_string())
        }
    }
}
