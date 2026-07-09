//! Send-app-card tool: deliver a structured mini-app payload to the current
//! channel. Capable clients (currently Robrix's agent-to-app type registry)
//! render this as a native GPU card; other clients fall back to the plain
//! `body` text.
//!
//! # Producer contract (v1)
//!
//! The outgoing Matrix event content must carry these fields when this tool
//! is used. Keeping the contract narrow lets the Robrix-side consumer (see
//! `specs/task-agent-to-app-system.spec.md` in the robrix2 repo) and the
//! OctOS-side producer evolve without drift:
//!
//! - `msgtype` — always `"m.text"`
//! - `body` — user-facing fallback text shown by clients that do not
//!   implement the app type registry (this is the tool's `body` parameter)
//! - `org.octos.app.type` — string key into the client-side type registry
//!   (e.g. `"weather"`)
//! - `org.octos.app.version` — integer schema version (v1 default: `1`)
//! - `org.octos.app.initial_state` — free-form object; the client-side
//!   factory for this `type` is responsible for validation and rendering
//!
//! Clients that do not recognise `type` or fail validation fall back to
//! plain `body` rendering with a warning log, per the master spec.
//!
//! # How this tool is wired
//!
//! `SendAppCardTool::execute` constructs an `OutboundMessage` whose
//! `metadata` field contains an `"org.octos.app"` object. The Matrix
//! channel (`octos-bus::matrix_channel`) extracts that key and copies it
//! verbatim into the outgoing Matrix event `content`. Other channels
//! (CLI / Telegram / Feishu) ignore the metadata and deliver only the
//! `body` text.

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use octos_core::OutboundMessage;
use serde::Deserialize;
use tokio::sync::mpsc;

use super::{Tool, ToolResult};

const APP_REPLY_TAGS: &[&str] = &["gateway", "app_reply"];
const APP_METADATA_KEY: &str = "org.octos.app";
const ACTIONS_METADATA_KEY: &str = "org.octos.actions";
const MISSION_ROOM_TYPE: &str = "mission_room";

/// Tool for delivering structured mini-app payloads to the current channel.
pub struct SendAppCardTool {
    out_tx: mpsc::Sender<OutboundMessage>,
    default_channel: std::sync::Mutex<String>,
    default_chat_id: std::sync::Mutex<String>,
}

impl SendAppCardTool {
    pub fn new(out_tx: mpsc::Sender<OutboundMessage>) -> Self {
        Self {
            out_tx,
            default_channel: std::sync::Mutex::new(String::new()),
            default_chat_id: std::sync::Mutex::new(String::new()),
        }
    }

    /// Create a new tool with channel/chat_id context pre-set (preferred
    /// for per-session instances — matches `MessageTool::with_context`).
    pub fn with_context(
        out_tx: mpsc::Sender<OutboundMessage>,
        channel: impl Into<String>,
        chat_id: impl Into<String>,
    ) -> Self {
        Self {
            out_tx,
            default_channel: std::sync::Mutex::new(channel.into()),
            default_chat_id: std::sync::Mutex::new(chat_id.into()),
        }
    }

    /// Update the default channel/chat_id context (called per inbound message).
    /// Same concurrency caveat as `MessageTool::set_context`: prefer
    /// `with_context` for per-session tool instances.
    pub fn set_context(&self, channel: &str, chat_id: &str) {
        *self
            .default_channel
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = channel.to_string();
        *self
            .default_chat_id
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = chat_id.to_string();
    }
}

#[derive(Deserialize)]
struct Input {
    /// App type registered in the client's type registry. Currently only
    /// `"weather"` and `"mission_room"` are supported by Robrix. Unknown
    /// types are rendered as plain `body` text on the client with a warning.
    r#type: String,
    /// Schema version for this type. Default: 1.
    #[serde(default = "default_version")]
    version: u32,
    /// Free-form JSON object validated by the client-side factory for
    /// this `type`. For `weather`, minimum required fields are
    /// `location` (string), `temp_c` (number), `condition` (one of
    /// sunny / cloudy / rainy / snowy / stormy / foggy).
    initial_state: serde_json::Value,
    /// Optional app scope. `mission_room` requires `room`.
    #[serde(default)]
    scope: Option<String>,
    /// Stable app instance id for room/account scoped apps.
    #[serde(default)]
    app_id: Option<String>,
    /// Optional shared action buttons projected into `org.octos.actions`.
    #[serde(default)]
    actions: Option<serde_json::Value>,
    /// User-facing fallback text for clients without the type registry
    /// (also shown in notifications and chat previews).
    body: String,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    chat_id: Option<String>,
}

fn default_version() -> u32 {
    1
}

#[async_trait]
impl Tool for SendAppCardTool {
    fn name(&self) -> &str {
        "send_app_card"
    }

    fn description(&self) -> &str {
        "Deliver a structured mini-app card (e.g. a weather card) to the \
         current chat. Capable clients (such as Robrix) render this as a \
         native GPU card; other clients fall back to the `body` text.\n\
         \n\
         CRITICAL — NEVER FABRICATE APP DATA:\n\
         You MUST populate `initial_state` from real data obtained via \
         other tools in a PRIOR tool call in the same turn. Do NOT fill \
         `initial_state` from your own training knowledge or guesses. If \
         you do not have real data, call the appropriate data-fetching \
         tool first, or fall back to the plain `message` tool with an \
         honest text reply.\n\
         \n\
         WEATHER TYPE (type=\"weather\"):\n\
         You MUST first call `get_weather(city)` (or `get_forecast(city, days)` \
         if the user asks for multi-day forecast). Parse the returned text \
         output and map it to the schema:\n\
         - `location`: the displayed city name from the `get_weather` output\n\
         - `temp_c`: the temperature number from `Temperature: X.X°C` line\n\
         - `feels_like_c` (optional): from `(feels like X.X°C)` if present\n\
         - `humidity` (optional): integer from `Humidity: X%` line\n\
         - `wind_kph` (optional): number from `Wind: X.X km/h` line\n\
         - `condition`: map the conditions text to ONE OF: `sunny`, `cloudy`, \
         `rainy`, `snowy`, `stormy`, `foggy`, using this table:\n\
           * `Clear sky`, `Mainly clear`, `Sunny` → `sunny`\n\
           * `Partly cloudy`, `Overcast`, `Cloudy` → `cloudy`\n\
           * `Drizzle`, `Rain`, `Rain showers`, `Freezing rain` → `rainy`\n\
           * `Snow`, `Snow showers`, `Snow fall` → `snowy`\n\
           * `Thunderstorm`, `Thunderstorm with hail` → `stormy`\n\
           * `Fog`, `Depositing rime fog`, `Mist`, `Haze` → `foggy`\n\
         - `forecast` (optional): if you called `get_forecast`, build an \
         array (max 7) of `{day, high_c, low_c, condition}` entries parsed \
         from each forecast day; otherwise omit the field.\n\
         - `body`: a concise one-line natural language summary for clients \
         without the app registry, e.g. `\"Beijing 22°C partly cloudy\"`.\n\
         \n\
         MISSION ROOM TYPE (type=\"mission_room\"):\n\
         You MUST set `scope` to `room` and provide a stable `app_id` such as \
         `mission.main`. Put the full shared mission snapshot in \
         `initial_state`, and put shared human controls in `actions` as an \
         array of `{id, label, style}` objects. Use this for room-scoped \
         planning, approval, execution, review, and blocker updates.\n\
         \n\
         If the user wrote a non-English city name, pass it to `get_weather` \
         in English (that tool requires English). Use the original language \
         of the user's request for the `body` fallback text and for `location` \
         if the get_weather output includes a localized name.\n\
         \n\
         Use this tool INSTEAD OF the plain `message` tool whenever the user \
         asks for data that has a registered app type AND you have real data \
         from a prior tool call to populate it. Never use this tool with \
         made-up `initial_state`."
    }

    fn tags(&self) -> &[&str] {
        APP_REPLY_TAGS
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "type": {
                    "type": "string",
                    "description": "App type key in the client's type registry. \
                        Supported v1 types: 'weather', 'mission_room'."
                },
                "version": {
                    "type": "integer",
                    "description": "Schema version. Defaults to 1.",
                    "default": 1
                },
                "initial_state": {
                    "type": "object",
                    "description": "Type-specific payload. For 'weather': \
                        {location: string, temp_c: number, condition: \
                        'sunny'|'cloudy'|'rainy'|'snowy'|'stormy'|'foggy', \
                        optional feels_like_c: number, humidity: integer 0..100, \
                        wind_kph: number >= 0, updated_at: RFC 3339 UTC string, \
                        forecast: array up to 7 of {day, high_c, low_c, condition}}."
                },
                "scope": {
                    "type": "string",
                    "description": "Optional app scope. Required as 'room' for \
                        mission_room and other room-scoped apps."
                },
                "app_id": {
                    "type": "string",
                    "description": "Stable app instance id. Required for \
                        room/account scoped apps, e.g. 'mission.main'."
                },
                "actions": {
                    "type": "array",
                    "description": "Optional shared OctOS action buttons. \
                        Each item should include id, label, and optional style."
                },
                "body": {
                    "type": "string",
                    "description": "User-facing fallback text. Shown verbatim \
                        by clients that don't implement the app type registry \
                        (CLI, Telegram, Feishu, plain Matrix clients). Must be a \
                        concise natural-language summary of the app data."
                },
                "channel": {
                    "type": "string",
                    "description": "Target channel. Defaults to current."
                },
                "chat_id": {
                    "type": "string",
                    "description": "Target chat/user ID. Defaults to current."
                }
            },
            "required": ["type", "initial_state", "body"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input =
            serde_json::from_value(args.clone()).wrap_err("invalid send_app_card tool input")?;

        let channel = input.channel.unwrap_or_else(|| {
            self.default_channel
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        });
        let chat_id = input.chat_id.unwrap_or_else(|| {
            self.default_chat_id
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        });

        if channel.is_empty() || chat_id.is_empty() {
            return Ok(ToolResult {
                output: "Error: No target channel/chat specified.".into(),
                success: false,
                ..Default::default()
            });
        }

        if input.body.trim().is_empty() {
            return Ok(ToolResult {
                output: "Error: body fallback text must not be empty.".into(),
                success: false,
                ..Default::default()
            });
        }

        if input.r#type.trim().is_empty() {
            return Ok(ToolResult {
                output: "Error: app type must not be empty.".into(),
                success: false,
                ..Default::default()
            });
        }

        if !input.initial_state.is_object() {
            return Ok(ToolResult {
                output: "Error: initial_state must be a JSON object.".into(),
                success: false,
                ..Default::default()
            });
        }

        let scope = input
            .scope
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let app_id = input
            .app_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if input.r#type == MISSION_ROOM_TYPE && (scope != Some("room") || app_id.is_none()) {
            return Ok(ToolResult {
                output: "Error: mission_room app cards require scope='room' and a stable app_id."
                    .into(),
                success: false,
                ..Default::default()
            });
        }
        if let Some(actions) = &input.actions
            && !actions.is_array()
        {
            return Ok(ToolResult {
                output: "Error: actions must be a JSON array.".into(),
                success: false,
                ..Default::default()
            });
        }

        // Build the metadata payload. The matrix channel extracts the
        // `"org.octos.app"` key and copies it verbatim into the outgoing
        // Matrix event content. Non-Matrix channels ignore the metadata
        // and send only `body`.
        let mut app = serde_json::json!({
            "type": input.r#type,
            "version": input.version,
            "initial_state": input.initial_state,
        });
        if let Some(scope) = scope {
            app["scope"] = serde_json::json!(scope);
        }
        if let Some(app_id) = app_id {
            app["app_id"] = serde_json::json!(app_id);
        }
        let mut metadata = serde_json::json!({
            APP_METADATA_KEY: app
        });
        if let Some(actions) = input.actions {
            metadata[ACTIONS_METADATA_KEY] = actions;
        }

        let msg = OutboundMessage {
            channel: channel.clone(),
            chat_id: chat_id.clone(),
            content: input.body,
            reply_to: None,
            media: vec![],
            metadata,
        };

        self.out_tx
            .send(msg)
            .await
            .map_err(|e| eyre::eyre!("failed to send app card message: {e}"))?;

        Ok(ToolResult {
            output: String::new(),
            success: true,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_tool() -> (SendAppCardTool, mpsc::Receiver<OutboundMessage>) {
        let (tx, rx) = mpsc::channel(4);
        (
            SendAppCardTool::with_context(tx, "matrix", "!room:example.org"),
            rx,
        )
    }

    #[tokio::test]
    async fn should_put_org_octos_app_in_metadata_when_weather_payload_valid() {
        let (tool, mut rx) = make_tool();
        let result = tool
            .execute(&json!({
                "type": "weather",
                "initial_state": {
                    "location": "Beijing",
                    "temp_c": 22,
                    "condition": "sunny"
                },
                "body": "Beijing 22°C sunny"
            }))
            .await
            .expect("tool run ok");
        assert!(result.success, "expected success, got: {}", result.output);

        let msg = rx.recv().await.expect("message should have been sent");
        assert_eq!(msg.channel, "matrix");
        assert_eq!(msg.chat_id, "!room:example.org");
        assert_eq!(msg.content, "Beijing 22°C sunny");
        let app = msg
            .metadata
            .get("org.octos.app")
            .expect("metadata should carry org.octos.app");
        assert_eq!(app.get("type"), Some(&json!("weather")));
        assert_eq!(app.get("version"), Some(&json!(1)));
        assert_eq!(
            app.get("initial_state")
                .and_then(|v| v.get("location"))
                .and_then(|v| v.as_str()),
            Some("Beijing"),
        );
    }

    #[tokio::test]
    async fn should_fail_when_body_missing() {
        let (tool, _rx) = make_tool();
        let result = tool
            .execute(&json!({
                "type": "weather",
                "initial_state": {"location": "Test", "temp_c": 0, "condition": "sunny"},
                "body": ""
            }))
            .await
            .expect("tool run ok");
        assert!(!result.success);
        assert!(result.output.contains("body"));
    }

    #[tokio::test]
    async fn should_fail_when_initial_state_not_object() {
        let (tool, _rx) = make_tool();
        let result = tool
            .execute(&json!({
                "type": "weather",
                "initial_state": "not-an-object",
                "body": "fallback"
            }))
            .await;
        // Serde accepts non-object initial_state because the struct field is
        // typed as Value, so it parses; our own validation catches it.
        let result = result.expect("tool run ok");
        assert!(!result.success);
        assert!(result.output.contains("initial_state"));
    }

    #[tokio::test]
    async fn should_default_version_to_1_when_omitted() {
        let (tool, mut rx) = make_tool();
        tool.execute(&json!({
            "type": "weather",
            "initial_state": {"location": "T", "temp_c": 1, "condition": "sunny"},
            "body": "b"
        }))
        .await
        .unwrap();
        let msg = rx.recv().await.unwrap();
        assert_eq!(
            msg.metadata
                .get("org.octos.app")
                .and_then(|v| v.get("version")),
            Some(&json!(1))
        );
    }

    #[tokio::test]
    async fn should_include_scope_app_id_and_actions_when_mission_room_payload_valid() {
        let (tool, mut rx) = make_tool();
        let result = tool
            .execute(&json!({
                "type": "mission_room",
                "version": 1,
                "scope": "room",
                "app_id": "mission.main",
                "initial_state": {
                    "goal": {"title": "Ship agent2view runtime", "status": "planning"},
                    "phase": "planning",
                    "tasks": [],
                    "agents": [],
                    "pending_human_actions": [
                        {"id": "approve_plan", "label": "Approve plan"}
                    ],
                    "decisions": [],
                    "blockers": []
                },
                "actions": [
                    {"id": "approve_plan", "label": "Approve plan", "style": "primary"},
                    {"id": "request_plan_changes", "label": "Request changes", "style": "secondary"}
                ],
                "body": "Mission update: plan is waiting for approval."
            }))
            .await
            .expect("tool run ok");
        assert!(result.success, "expected success, got: {}", result.output);

        let msg = rx.recv().await.expect("message should have been sent");
        let app = msg
            .metadata
            .get("org.octos.app")
            .expect("metadata should carry org.octos.app");
        assert_eq!(app.get("type"), Some(&json!("mission_room")));
        assert_eq!(app.get("scope"), Some(&json!("room")));
        assert_eq!(app.get("app_id"), Some(&json!("mission.main")));
        assert_eq!(msg.metadata["org.octos.actions"][0]["id"], "approve_plan");
    }

    #[tokio::test]
    async fn should_fail_when_mission_room_lacks_room_scope_or_app_id() {
        let (tool, _rx) = make_tool();
        let result = tool
            .execute(&json!({
                "type": "mission_room",
                "initial_state": {},
                "body": "Mission update"
            }))
            .await
            .expect("tool run ok");

        assert!(!result.success);
        assert!(result.output.contains("mission_room"));
        assert!(result.output.contains("scope"));
    }
}
