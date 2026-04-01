//! WeChat Bridge — persistent subprocess that maintains the WeChat long-poll connection.
//!
//! Architecture:
//!   WeChat server <-(HTTP long-poll)-> this bridge <-(WebSocket)-> octos gateway channel
//!
//! The bridge never restarts, so the WeChat session stays alive even when the gateway restarts.
//!
//! Protocol (WebSocket JSON messages):
//!   Bridge → Client: {"type":"message","sender":"xxx@im.wechat","content":"hello","context_token":"...","message_id":"123"}
//!   Client → Bridge: {"type":"send","to":"xxx@im.wechat","text":"reply","context_token":"..."}
//!
//! Stdout events (read by ProcessManager):
//!   {"type":"qr","qr_url":"https://..."}
//!   {"type":"status","status":"connected","token":"..."}
//!   {"type":"status","status":"session_timeout"}
//!   {"type":"status","status":"polling"}

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, RwLock, broadcast};
use tokio_tungstenite::tungstenite::Message as WsMessage;

const WECHAT_API_BASE: &str = "https://ilinkai.weixin.qq.com";
const LONG_POLL_TIMEOUT_SECS: u64 = 40;

#[derive(Parser)]
#[command(name = "wechat-bridge")]
struct Args {
    /// WebSocket server port
    #[arg(long, default_value = "3201")]
    port: u16,

    /// Bot token (if empty, starts QR login flow)
    #[arg(long, default_value = "")]
    token: String,

    /// WeChat API base URL
    #[arg(long, default_value = WECHAT_API_BASE)]
    base_url: String,
}

/// Emit a JSON event to stdout (read by ProcessManager).
fn emit(event: &serde_json::Value) {
    println!("{}", event);
}

struct BridgeState {
    token: RwLock<String>,
    base_url: String,
    get_updates_buf: Mutex<String>,
    /// Broadcast channel for sending messages from bridge to all connected WS clients.
    inbound_tx: broadcast::Sender<String>,
    /// Pending outbound sends (context_token keyed by user_id, for replies).
    context_tokens: RwLock<HashMap<String, String>>,
    http: reqwest::Client,
}

impl BridgeState {
    fn new(token: String, base_url: String) -> Arc<Self> {
        let (inbound_tx, _) = broadcast::channel(256);
        Arc::new(Self {
            token: RwLock::new(token),
            base_url,
            get_updates_buf: Mutex::new(String::new()),
            inbound_tx,
            context_tokens: RwLock::new(HashMap::new()),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(LONG_POLL_TIMEOUT_SECS + 10))
                .build()
                .unwrap_or_default(),
        })
    }

    fn headers(&self, token: &str) -> Vec<(&'static str, String)> {
        vec![
            ("Content-Type", "application/json".into()),
            ("Authorization", format!("Bearer {token}")),
            ("AuthorizationType", "ilink_bot_token".into()),
            ("X-WECHAT-UIN", "MTIzNA==".into()),
        ]
    }

    async fn send_to_wechat(
        &self,
        to: &str,
        text: &str,
        context_token: &str,
    ) -> Result<(), String> {
        let token = self.token.read().await.clone();
        let client_id = format!(
            "octos-{:016x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64
        );

        let body = json!({
            "msg": {
                "from_user_id": "",
                "to_user_id": to,
                "client_id": client_id,
                "message_type": 2,
                "message_state": 2,
                "item_list": [{"type": 1, "text_item": {"text": text}}],
                "context_token": context_token,
            },
            "base_info": {"channel_version": "1.0.0"}
        });

        let mut req = self
            .http
            .post(format!("{}/ilink/bot/sendmessage", self.base_url))
            .timeout(std::time::Duration::from_secs(15));
        for (k, v) in self.headers(&token) {
            req = req.header(k, v);
        }
        req.json(&body)
            .send()
            .await
            .map_err(|e| format!("send failed: {e}"))?;
        Ok(())
    }
}

/// QR login flow: get QR code, poll until confirmed, return token.
async fn qr_login(base_url: &str) -> Result<String, String> {
    let client = reqwest::Client::new();

    loop {
        // Get QR code
        let resp = client
            .get(format!("{base_url}/ilink/bot/get_bot_qrcode?bot_type=3"))
            .send()
            .await
            .map_err(|e| format!("QR fetch failed: {e}"))?;
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("QR parse failed: {e}"))?;

        let qrcode = body["qrcode"].as_str().unwrap_or_default().to_string();
        let qr_url = body["qrcode_img_content"]
            .as_str()
            .unwrap_or_default()
            .to_string();

        emit(&json!({"type": "qr", "qr_url": qr_url}));
        eprintln!("[wechat-bridge] QR code: {qr_url}");

        // Poll status
        let poll_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(40))
            .build()
            .unwrap_or_default();

        loop {
            let url = format!("{base_url}/ilink/bot/get_qrcode_status?qrcode={qrcode}");
            let resp = match poll_client
                .get(&url)
                .header("iLink-App-ClientVersion", "1")
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) if e.is_timeout() => continue,
                Err(e) => return Err(format!("QR poll failed: {e}")),
            };

            let body: serde_json::Value = resp
                .json()
                .await
                .map_err(|e| format!("QR poll parse: {e}"))?;
            let status = body["status"].as_str().unwrap_or("wait");

            match status {
                "wait" => continue,
                "scaned" => {
                    eprintln!("[wechat-bridge] QR scanned, waiting for confirm...");
                    emit(&json!({"type": "status", "status": "scanned"}));
                }
                "confirmed" => {
                    let token = body["bot_token"].as_str().unwrap_or_default().to_string();
                    eprintln!("[wechat-bridge] Login confirmed!");
                    emit(&json!({"type": "status", "status": "connected", "token": token}));
                    return Ok(token);
                }
                "expired" => {
                    eprintln!("[wechat-bridge] QR expired, refreshing...");
                    break; // outer loop will get new QR
                }
                other => {
                    eprintln!("[wechat-bridge] Unknown QR status: {other}");
                }
            }

            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }
}

/// Long-poll loop: getUpdates from WeChat, broadcast to WS clients.
async fn poll_loop(state: Arc<BridgeState>) {
    emit(&json!({"type": "status", "status": "polling"}));
    eprintln!("[wechat-bridge] Starting long-poll loop");

    loop {
        let token = state.token.read().await.clone();
        let buf = state.get_updates_buf.lock().await.clone();

        let body = json!({
            "get_updates_buf": buf,
            "base_info": {"channel_version": "1.0.0"}
        });

        let mut req = state
            .http
            .post(format!("{}/ilink/bot/getupdates", state.base_url))
            .timeout(std::time::Duration::from_secs(LONG_POLL_TIMEOUT_SECS));
        for (k, v) in state.headers(&token) {
            req = req.header(k, v);
        }

        let result = req.json(&body).send().await;

        match result {
            Ok(resp) => {
                let text = match resp.text().await {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let data: serde_json::Value = match serde_json::from_str(&text) {
                    Ok(d) => d,
                    Err(_) => continue,
                };

                // Update cursor
                if let Some(new_buf) = data["get_updates_buf"].as_str() {
                    if !new_buf.is_empty() {
                        *state.get_updates_buf.lock().await = new_buf.to_string();
                    }
                }

                let errcode = data["errcode"].as_i64().unwrap_or(0);
                if errcode == -14 {
                    eprintln!("[wechat-bridge] Session timeout, resetting buf");
                    emit(&json!({"type": "status", "status": "session_timeout"}));
                    *state.get_updates_buf.lock().await = String::new();
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    continue;
                } else if errcode != 0 {
                    eprintln!("[wechat-bridge] getUpdates error: {errcode}");
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    continue;
                }

                if let Some(msgs) = data["msgs"].as_array() {
                    for msg in msgs {
                        let msg_type = msg["message_type"].as_u64().unwrap_or(0);
                        if msg_type != 1 {
                            continue;
                        } // only user messages

                        let sender = msg["from_user_id"].as_str().unwrap_or_default();
                        let ctx = msg["context_token"].as_str().unwrap_or_default();
                        let msg_id = msg["message_id"].as_u64().unwrap_or(0).to_string();

                        // Store context_token
                        if !ctx.is_empty() {
                            state
                                .context_tokens
                                .write()
                                .await
                                .insert(sender.to_string(), ctx.to_string());
                        }

                        // Extract text
                        let mut text = String::new();
                        if let Some(items) = msg["item_list"].as_array() {
                            for item in items {
                                if item["type"].as_u64() == Some(1) {
                                    if let Some(t) = item["text_item"]["text"].as_str() {
                                        text = t.to_string();
                                    }
                                }
                            }
                        }

                        if text.trim().is_empty() {
                            continue;
                        }

                        eprintln!("[wechat-bridge] recv from={sender} text={text}");

                        // Broadcast to WS clients
                        let event = json!({
                            "type": "message",
                            "sender": sender,
                            "content": text,
                            "context_token": ctx,
                            "message_id": msg_id,
                        });
                        let _ = state.inbound_tx.send(event.to_string());
                    }
                }
            }
            Err(e) => {
                if e.is_timeout() {
                    continue;
                }
                eprintln!("[wechat-bridge] poll error: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        }
    }
}

/// Handle a single WebSocket client connection.
async fn handle_ws_client(stream: TcpStream, state: Arc<BridgeState>) {
    let ws = match tokio_tungstenite::accept_async(stream).await {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("[wechat-bridge] WS accept error: {e}");
            return;
        }
    };

    let (mut ws_tx, mut ws_rx) = ws.split();
    let mut inbound_rx = state.inbound_tx.subscribe();

    eprintln!("[wechat-bridge] WS client connected");

    loop {
        tokio::select! {
            // Forward inbound WeChat messages to this WS client
            Ok(msg) = inbound_rx.recv() => {
                if ws_tx.send(WsMessage::Text(msg.into())).await.is_err() {
                    break;
                }
            }
            // Handle outbound messages from WS client (gateway wants to send)
            frame = ws_rx.next() => {
                match frame {
                    Some(Ok(WsMessage::Text(text))) => {
                        if let Ok(cmd) = serde_json::from_str::<serde_json::Value>(&text) {
                            if cmd["type"].as_str() == Some("send") {
                                let to = cmd["to"].as_str().unwrap_or_default();
                                let text = cmd["text"].as_str().unwrap_or_default();
                                let ctx = cmd["context_token"].as_str()
                                    .filter(|s| !s.is_empty())
                                    .map(|s| s.to_string());

                                // Use provided context_token, or look up stored one
                                let context_token = match ctx {
                                    Some(ct) => ct,
                                    None => state.context_tokens.read().await
                                        .get(to).cloned().unwrap_or_default(),
                                };

                                if let Err(e) = state.send_to_wechat(to, text, &context_token).await {
                                    eprintln!("[wechat-bridge] send error: {e}");
                                }
                            }
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | None => break,
                    _ => {}
                }
            }
        }
    }

    eprintln!("[wechat-bridge] WS client disconnected");
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // Get token: either from args or via QR login
    let token = if args.token.is_empty() {
        match qr_login(&args.base_url).await {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[wechat-bridge] QR login failed: {e}");
                std::process::exit(1);
            }
        }
    } else {
        args.token.clone()
    };

    let state = BridgeState::new(token, args.base_url);

    // Start WebSocket server
    let addr = SocketAddr::from(([127, 0, 0, 1], args.port));
    let listener = TcpListener::bind(&addr)
        .await
        .expect("failed to bind WS port");
    eprintln!(
        "[wechat-bridge] WebSocket server on ws://localhost:{}",
        args.port
    );

    // Start long-poll loop
    let poll_state = state.clone();
    tokio::spawn(async move { poll_loop(poll_state).await });

    // Accept WS clients
    while let Ok((stream, _)) = listener.accept().await {
        let client_state = state.clone();
        tokio::spawn(async move { handle_ws_client(stream, client_state).await });
    }
}
