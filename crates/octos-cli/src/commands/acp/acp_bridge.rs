//! ACP Bridge implementation for gateway communication.

use eyre::Result;
use futures_util::{Sink, SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{self, Message},
};
use tracing::{debug, error, info, warn};
use url::Url;

/// Maximum number of redirects to follow
const MAX_REDIRECTS: u8 = 5;

/// Connect to a WebSocket URL, following HTTP redirects (301, 302, 307, 308).
async fn connect_with_redirects(
    url: &str,
) -> Result<(
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    String,
)> {
    let mut current_url = url.to_string();
    println!("Connecting to WebSocket URL: {}", current_url);
    for redirect_count in 0..MAX_REDIRECTS {
        debug!(
            "WebSocket connection attempt {} to {}",
            redirect_count + 1,
            current_url
        );

        match connect_async(&current_url).await {
            Ok((stream, _response)) => {
                return Ok((stream, current_url));
            }
            Err(tungstenite::Error::Http(response)) => {
                let status = response.status();
                if status.is_redirection() {
                    // Extract Location header for redirect
                    if let Some(location) = response.headers().get("location") {
                        let location_str = location
                            .to_str()
                            .map_err(|e| eyre::eyre!("Invalid Location header: {}", e))?;

                        // Handle relative vs absolute URLs
                        let new_url = if location_str.starts_with("ws://")
                            || location_str.starts_with("wss://")
                            || location_str.starts_with("http://")
                            || location_str.starts_with("https://")
                        {
                            // Convert http(s) to ws(s) if needed
                            location_str
                                .replace("http://", "ws://")
                                .replace("https://", "wss://")
                        } else {
                            // Relative URL - resolve against current URL
                            let base = Url::parse(&current_url)?;
                            base.join(location_str)?.to_string()
                        };

                        info!(
                            "Following {} redirect from {} to {}",
                            status.as_u16(),
                            current_url,
                            new_url
                        );
                        current_url = new_url;
                        continue;
                    } else {
                        return Err(eyre::eyre!(
                            "Redirect response {} without Location header",
                            status
                        ));
                    }
                } else {
                    return Err(eyre::eyre!("HTTP error: {}", status));
                }
            }
            Err(e) => {
                return Err(eyre::eyre!("WebSocket connection error: {}", e));
            }
        }
    }

    Err(eyre::eyre!(
        "Too many redirects (max {})",
        MAX_REDIRECTS
    ))
}

/// Configuration for the ACP bridge
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    pub gateway_url: Url,
    pub token: Option<String>,
    pub password: Option<String>,
    pub token_file: Option<PathBuf>,
    pub password_file: Option<PathBuf>,
    pub session_key: Option<String>,
    pub session_label: Option<String>,
    pub verbose: bool,
    pub require_existing: bool,
    pub reset_session: bool,
    pub no_prefix_cwd: bool,
}

/// ACP message types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AcpMessage {
    Request {
        id: String,
        method: String,
        params: serde_json::Value,
    },
    Response {
        id: String,
        result: Option<serde_json::Value>,
        error: Option<AcpError>,
    },
    Notification {
        method: String,
        params: serde_json::Value,
    },
    Error {
        code: i32,
        message: String,
        data: Option<serde_json::Value>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpError {
    pub code: i32,
    pub message: String,
    pub data: Option<serde_json::Value>,
}

/// Main ACP Bridge implementation
pub struct AcpBridge {
    config: BridgeConfig,
    #[allow(dead_code)]
    session_state: HashMap<String, serde_json::Value>,
}

impl AcpBridge {
    pub fn new(config: BridgeConfig) -> Self {
        Self {
            config,
            session_state: HashMap::new(),
        }
    }

    /// Initialize the bridge and validate configuration
    pub async fn initialize(&mut self) -> Result<()> {
        info!("Initializing ACP bridge");

        // Validate configuration
        self.validate_config().await?;

        // Load authentication credentials
        self.load_credentials().await?;

        // Initialize session if specified
        if let Some(session_key) = self.config.session_key.clone() {
            self.initialize_session(&session_key).await?;
        }

        Ok(())
    }

    /// Run the interactive ACP client
    pub async fn run_client(&mut self) -> Result<()> {
        info!("Starting ACP client");

        // Establish WebSocket connection to gateway (with redirect handling)
        let (ws_stream, final_url) =
            connect_with_redirects(self.config.gateway_url.as_str()).await?;
        info!("Connected to gateway: {}", final_url);

        // Split stream for reading and writing
        let (mut ws_sender, mut ws_receiver) = ws_stream.split();

        // Start message handling loop
        loop {
            tokio::select! {
                // Handle incoming messages from gateway
                msg = ws_receiver.next() => {
                    if let Some(msg) = msg {
                        match msg? {
                            Message::Text(text) => {
                                if let Err(e) = self.handle_gateway_message(&text).await {
                                    error!("Error handling gateway message: {}", e);
                                }
                            }
                            Message::Binary(data) => {
                                warn!("Received binary message, ignoring: {} bytes", data.len());
                            }
                            Message::Close(_) => {
                                info!("Gateway connection closed");
                                break;
                            }
                            _ => {}
                        }
                    }
                }

                // Handle user input (if in interactive mode)
                input = self.read_user_input() => {
                    if let Some(input) = input {
                        if input.trim() == "quit" || input.trim() == "exit" {
                            info!("User requested exit");
                            break;
                        }

                        if let Err(e) = self.handle_user_input(&input, &mut ws_sender).await {
                            error!("Error handling user input: {}", e);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn validate_config(&self) -> Result<()> {
        // Validate gateway URL
        if self.config.gateway_url.scheme() != "ws"
            && self.config.gateway_url.scheme() != "wss"
        {
            return Err(eyre::eyre!(
                "Gateway URL must use ws:// or wss:// scheme"
            ));
        }

        // Validate session key format if provided
        if let Some(session_key) = &self.config.session_key {
            if !session_key.starts_with("agent:") {
                warn!("Session key should follow format 'agent:namespace:name'");
            }
        }

        Ok(())
    }

    async fn load_credentials(&mut self) -> Result<()> {
        // Load token from file if specified
        if let Some(token_file) = &self.config.token_file {
            if let Ok(token) = tokio::fs::read_to_string(token_file).await {
                self.config.token = Some(token.trim().to_string());
                debug!("Loaded token from file: {:?}", token_file);
            }
        }

        // Load password from file if specified
        if let Some(password_file) = &self.config.password_file {
            if let Ok(password) = tokio::fs::read_to_string(password_file).await {
                self.config.password = Some(password.trim().to_string());
                debug!("Loaded password from file: {:?}", password_file);
            }
        }

        Ok(())
    }

    async fn initialize_session(&mut self, session_key: &str) -> Result<()> {
        info!("Initializing session: {}", session_key);

        if self.config.reset_session {
            info!("Resetting session before use");
            // Logic to reset session would go here
        }

        if self.config.require_existing {
            // Logic to check if session exists would go here
            debug!("Checking if session exists: {}", session_key);
        }

        Ok(())
    }

    async fn handle_gateway_message(&mut self, message: &str) -> Result<()> {
        if self.config.verbose {
            debug!("Received from gateway: {}", message);
        }

        // Parse ACP message
        let acp_message: AcpMessage = serde_json::from_str(message)?;

        match acp_message {
            AcpMessage::Request {
                id,
                method,
                params: _,
            } => {
                info!("Received request: {} ({})", method, id);
                // Handle request logic here
            }
            AcpMessage::Response { id, result: _, error } => {
                if let Some(error) = error {
                    warn!("Received error response {}: {}", id, error.message);
                } else {
                    info!("Received response: {}", id);
                }
                // Handle response logic here
            }
            AcpMessage::Notification { method, params: _ } => {
                info!("Received notification: {}", method);
                // Handle notification logic here
            }
            AcpMessage::Error { code, message, .. } => {
                error!("Received error: {} - {}", code, message);
            }
        }

        Ok(())
    }

    async fn read_user_input(&self) -> Option<String> {
        use tokio::io::{AsyncBufReadExt, BufReader};

        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        let mut line = String::new();

        // Print prompt if not suppressed
        if !self.config.no_prefix_cwd {
            if let Ok(cwd) = std::env::current_dir() {
                eprint!("{} > ", cwd.display());
            } else {
                eprint!("> ");
            }
        } else {
            eprint!("> ");
        }

        match reader.read_line(&mut line).await {
            Ok(0) => None, // EOF
            Ok(_) => Some(line),
            Err(e) => {
                error!("Error reading stdin: {}", e);
                None
            }
        }
    }

    async fn handle_user_input<S>(&mut self, input: &str, ws_sender: &mut S) -> Result<()>
    where
        S: Sink<Message> + Unpin,
        S::Error: std::error::Error + Send + Sync + 'static,
    {
        if input.trim().is_empty() {
            return Ok(());
        }

        // Parse user input as ACP command
        let message = AcpMessage::Request {
            id: uuid::Uuid::new_v4().to_string(),
            method: "user_input".to_string(),
            params: serde_json::json!({
                "input": input.trim(),
                "working_directory": if self.config.no_prefix_cwd {
                    None
                } else {
                    std::env::current_dir().ok()
                }
            }),
        };

        let json = serde_json::to_string(&message)?;
        if self.config.verbose {
            debug!("Sending to gateway: {}", json);
        }

        // Send message to gateway
        ws_sender
            .send(Message::Text(json.into()))
            .await
            .map_err(|e| eyre::eyre!("Failed to send message: {}", e))?;

        Ok(())
    }
}

impl BridgeConfig {
    /// Load configuration from environment and defaults
    pub fn from_env() -> Self {
        Self {
            gateway_url: std::env::var("GATEWAY_URL")
                .unwrap_or_else(|_| "ws://localhost:8080/acp".to_string())
                .parse()
                .expect("Invalid gateway URL"),
            token: std::env::var("GATEWAY_TOKEN").ok(),
            password: std::env::var("GATEWAY_PASSWORD").ok(),
            token_file: std::env::var("GATEWAY_TOKEN_FILE").ok().map(PathBuf::from),
            password_file: std::env::var("GATEWAY_PASSWORD_FILE")
                .ok()
                .map(PathBuf::from),
            session_key: std::env::var("ACP_SESSION").ok(),
            session_label: std::env::var("ACP_SESSION_LABEL").ok(),
            verbose: std::env::var("VERBOSE").is_ok(),
            require_existing: false,
            reset_session: false,
            no_prefix_cwd: false,
        }
    }
}