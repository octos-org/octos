//! Feishu/Lark channel with WebSocket long connection or Webhook mode + REST API.
//!
//! Supports both Feishu (China, open.feishu.cn) and Larksuite (global, open.larksuite.com).
//! Set `region` to `"cn"` or `"global"` to select the platform.
//! Set `mode` to `"ws"` (default) for WebSocket long connection or `"webhook"` for HTTP webhook.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use chrono::Utc;
use eyre::{Result, WrapErr};
use futures::StreamExt;
use octos_core::{InboundMessage, OutboundMessage};
use reqwest::Client;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, error, info, warn};

// Re-used for explicit TLS connector (avoids CryptoProvider auto-detection panic).
extern crate rustls;
extern crate rustls_native_certs;

use crate::channel::Channel;
use crate::dedup::MessageDedup;
use crate::media::{download_media, is_image};

use futures::SinkExt;

/// Token refresh interval (slightly under 2 hours).
const TOKEN_TTL_SECS: u64 = 7000;
/// Maximum message IDs to track for dedup.
const MAX_SEEN_IDS: usize = 1000;
/// Default ping interval for Feishu WS (2 minutes, matching official SDK).
const FEISHU_PING_INTERVAL_SECS: u64 = 120;

// --- Feishu WebSocket binary frame protocol (protobuf2) ---

/// Frame type: control (ping/pong).
const FRAME_TYPE_CONTROL: i32 = 0;
/// Frame type: data (events/cards).
const FRAME_TYPE_DATA: i32 = 1;

/// Header key constants matching the official SDK.
const HEADER_TYPE: &str = "type";
const HEADER_MESSAGE_ID: &str = "message_id";
const HEADER_SUM: &str = "sum";
const HEADER_SEQ: &str = "seq";
const HEADER_BIZ_RT: &str = "biz_rt";

/// Message type constants.
const MSG_TYPE_EVENT: &str = "event";
const MSG_TYPE_PING: &str = "ping";
const MSG_TYPE_PONG: &str = "pong";

/// A key-value header pair in a Feishu WS frame.
#[derive(Debug, Clone)]
struct FrameHeader {
    key: String,
    value: String,
}

/// Binary frame used by the Feishu WebSocket protocol.
/// Protobuf2 wire format with fields:
///   1: SeqID (varint), 2: LogID (varint), 3: service (varint),
///   4: method (varint), 5: headers (length-delimited, repeated),
///   6: payload_encoding (length-delimited), 7: payload_type (length-delimited),
///   8: payload (length-delimited), 9: LogIDNew (length-delimited)
#[derive(Debug, Clone)]
struct Frame {
    method: i32,
    headers: Vec<FrameHeader>,
    payload: Vec<u8>,
    service: i32,
    seq_id: u64,
    log_id: u64,
    log_id_new: String,
}

impl Frame {
    fn get_header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|h| h.key == key)
            .map(|h| h.value.as_str())
    }

    fn get_header_int(&self, key: &str) -> i32 {
        self.get_header(key)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    /// Decode a Frame from protobuf2 wire format.
    fn decode(buf: &[u8]) -> Result<Self> {
        let mut frame = Frame {
            method: 0,
            headers: Vec::new(),
            payload: Vec::new(),
            service: 0,
            seq_id: 0,
            log_id: 0,
            log_id_new: String::new(),
        };
        let mut pos = 0;
        while pos < buf.len() {
            let (tag, new_pos) = decode_varint(buf, pos)?;
            pos = new_pos;
            let field_number = (tag >> 3) as u32;
            let wire_type = (tag & 0x7) as u8;
            match (field_number, wire_type) {
                (1, 0) => {
                    // SeqID: varint
                    let (v, p) = decode_varint(buf, pos)?;
                    frame.seq_id = v;
                    pos = p;
                }
                (2, 0) => {
                    // LogID: varint
                    let (v, p) = decode_varint(buf, pos)?;
                    frame.log_id = v;
                    pos = p;
                }
                (3, 0) => {
                    // service: varint
                    let (v, p) = decode_varint(buf, pos)?;
                    frame.service = v as i32;
                    pos = p;
                }
                (4, 0) => {
                    // method: varint
                    let (v, p) = decode_varint(buf, pos)?;
                    frame.method = v as i32;
                    pos = p;
                }
                (5, 2) => {
                    // headers: length-delimited (embedded Header message)
                    let (data, p) = decode_bytes(buf, pos)?;
                    pos = p;
                    let header = decode_header(data)?;
                    frame.headers.push(header);
                }
                (6, 2) => {
                    // payload_encoding: length-delimited string (skip)
                    let (_, p) = decode_bytes(buf, pos)?;
                    pos = p;
                }
                (7, 2) => {
                    // payload_type: length-delimited string (skip)
                    let (_, p) = decode_bytes(buf, pos)?;
                    pos = p;
                }
                (8, 2) => {
                    // payload: length-delimited bytes
                    let (data, p) = decode_bytes(buf, pos)?;
                    frame.payload = data.to_vec();
                    pos = p;
                }
                (9, 2) => {
                    // LogIDNew: length-delimited string
                    let (data, p) = decode_bytes(buf, pos)?;
                    frame.log_id_new = String::from_utf8(data.to_vec()).unwrap_or_default();
                    pos = p;
                }
                (_, 0) => {
                    // Unknown varint — skip
                    let (_, p) = decode_varint(buf, pos)?;
                    pos = p;
                }
                (_, 2) => {
                    // Unknown length-delimited — skip
                    let (_, p) = decode_bytes(buf, pos)?;
                    pos = p;
                }
                _ => {
                    return Err(eyre::eyre!(
                        "unsupported protobuf wire type {wire_type} for field {field_number}"
                    ));
                }
            }
        }
        Ok(frame)
    }

    /// Encode this Frame to protobuf2 wire format.
    fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        if self.seq_id != 0 {
            encode_varint_field(&mut buf, 1, self.seq_id);
        }
        if self.log_id != 0 {
            encode_varint_field(&mut buf, 2, self.log_id);
        }
        if self.service != 0 {
            encode_varint_field(&mut buf, 3, self.service as u64);
        }
        encode_varint_field(&mut buf, 4, self.method as u64);
        for h in &self.headers {
            let header_bytes = encode_header(h);
            encode_bytes_field(&mut buf, 5, &header_bytes);
        }
        if !self.payload.is_empty() {
            encode_bytes_field(&mut buf, 8, &self.payload);
        }
        if !self.log_id_new.is_empty() {
            encode_bytes_field(&mut buf, 9, self.log_id_new.as_bytes());
        }
        buf
    }
}

fn decode_varint(buf: &[u8], start: usize) -> Result<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    let mut pos = start;
    loop {
        if pos >= buf.len() {
            return Err(eyre::eyre!("unexpected end of varint"));
        }
        let b = buf[pos];
        pos += 1;
        result |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok((result, pos));
        }
        shift += 7;
        if shift >= 64 {
            return Err(eyre::eyre!("varint too long"));
        }
    }
}

fn decode_bytes(buf: &[u8], start: usize) -> Result<(&[u8], usize)> {
    let (len, pos) = decode_varint(buf, start)?;
    let len = len as usize;
    if pos + len > buf.len() {
        return Err(eyre::eyre!("length-delimited field exceeds buffer"));
    }
    Ok((&buf[pos..pos + len], pos + len))
}

fn decode_header(buf: &[u8]) -> Result<FrameHeader> {
    let mut key = String::new();
    let mut value = String::new();
    let mut pos = 0;
    while pos < buf.len() {
        let (tag, new_pos) = decode_varint(buf, pos)?;
        pos = new_pos;
        let field_number = (tag >> 3) as u32;
        let wire_type = (tag & 0x7) as u8;
        if wire_type != 2 {
            // skip unknown varint
            let (_, p) = decode_varint(buf, pos)?;
            pos = p;
            continue;
        }
        let (data, p) = decode_bytes(buf, pos)?;
        pos = p;
        match field_number {
            1 => key = String::from_utf8(data.to_vec()).unwrap_or_default(),
            2 => value = String::from_utf8(data.to_vec()).unwrap_or_default(),
            _ => {}
        }
    }
    Ok(FrameHeader { key, value })
}

fn encode_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        if v < 0x80 {
            buf.push(v as u8);
            return;
        }
        buf.push((v as u8 & 0x7F) | 0x80);
        v >>= 7;
    }
}

fn encode_varint_field(buf: &mut Vec<u8>, field: u32, value: u64) {
    encode_varint(buf, ((field as u64) << 3) | 0); // wire type 0
    encode_varint(buf, value);
}

fn encode_bytes_field(buf: &mut Vec<u8>, field: u32, data: &[u8]) {
    encode_varint(buf, ((field as u64) << 3) | 2); // wire type 2
    encode_varint(buf, data.len() as u64);
    buf.extend_from_slice(data);
}

fn encode_header(h: &FrameHeader) -> Vec<u8> {
    let mut buf = Vec::new();
    encode_bytes_field(&mut buf, 1, h.key.as_bytes());
    encode_bytes_field(&mut buf, 2, h.value.as_bytes());
    buf
}

/// Build a ping frame for the given service_id.
fn new_ping_frame(service_id: i32) -> Frame {
    Frame {
        method: FRAME_TYPE_CONTROL,
        service: service_id,
        headers: vec![FrameHeader {
            key: HEADER_TYPE.to_string(),
            value: MSG_TYPE_PING.to_string(),
        }],
        payload: Vec::new(),
        seq_id: 0,
        log_id: 0,
        log_id_new: String::new(),
    }
}

/// Build a response frame echoing the incoming frame's metadata.
fn new_response_frame(incoming: &Frame, status_code: i32, biz_rt_ms: u64) -> Frame {
    let mut headers = incoming.headers.clone();
    headers.push(FrameHeader {
        key: HEADER_BIZ_RT.to_string(),
        value: biz_rt_ms.to_string(),
    });
    let payload = serde_json::json!({
        "StatusCode": status_code,
        "headers": {},
        "data": null,
    });
    Frame {
        method: incoming.method,
        service: incoming.service,
        headers,
        payload: payload.to_string().into_bytes(),
        seq_id: incoming.seq_id,
        log_id: incoming.log_id,
        log_id_new: incoming.log_id_new.clone(),
    }
}

fn base_url_for_region(region: &str) -> String {
    match region {
        "global" | "lark" => "https://open.larksuite.com/open-apis".to_string(),
        _ => "https://open.feishu.cn/open-apis".to_string(),
    }
}

/// Domain root (no /open-apis) — used for the WebSocket gateway endpoint.
fn domain_for_region(region: &str) -> String {
    match region {
        "global" | "lark" => "https://open.larksuite.com".to_string(),
        _ => "https://open.feishu.cn".to_string(),
    }
}

/// AES-256-CBC decryption for Lark encrypted events.
fn decrypt_lark_event(encrypt_key: &str, ciphertext_b64: &str) -> Result<String> {
    let key_hash = sha256(encrypt_key.as_bytes());

    let buf = base64_decode(ciphertext_b64).wrap_err("base64 decode failed")?;
    if buf.len() < 16 {
        return Err(eyre::eyre!("ciphertext too short"));
    }

    let iv = &buf[..16];
    let data = &buf[16..];
    if data.len() % 16 != 0 {
        return Err(eyre::eyre!("ciphertext not aligned to block size"));
    }

    let mut plaintext = data.to_vec();
    aes256_cbc_decrypt(&key_hash, iv, &mut plaintext)?;

    // PKCS7 unpad
    if let Some(&pad_len) = plaintext.last() {
        let pad_len = pad_len as usize;
        if pad_len > 0 && pad_len <= 16 && plaintext.len() >= pad_len {
            plaintext.truncate(plaintext.len() - pad_len);
        }
    }

    String::from_utf8(plaintext).wrap_err("decrypted data not valid UTF-8")
}

/// SHA-256 hash using the `sha2` crate.
fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// Minimal base64 decode (standard alphabet with padding).
fn base64_decode(input: &str) -> Result<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    fn val(c: u8) -> Result<u8> {
        if c == b'=' {
            return Ok(0);
        }
        TABLE
            .iter()
            .position(|&x| x == c)
            .map(|p| p as u8)
            .ok_or_else(|| eyre::eyre!("invalid base64 character"))
    }

    let input = input.trim();
    let bytes: Vec<u8> = input
        .bytes()
        .filter(|&b| b != b'\n' && b != b'\r')
        .collect();
    if bytes.len() % 4 != 0 {
        return Err(eyre::eyre!("invalid base64 length"));
    }

    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let a = val(chunk[0])?;
        let b = val(chunk[1])?;
        let c = val(chunk[2])?;
        let d = val(chunk[3])?;

        out.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' {
            out.push((b << 4) | (c >> 2));
        }
        if chunk[3] != b'=' {
            out.push((c << 6) | d);
        }
    }
    Ok(out)
}

/// AES-256-CBC decrypt in place (PKCS7 padding NOT removed).
fn aes256_cbc_decrypt(key: &[u8; 32], iv: &[u8], data: &mut [u8]) -> Result<()> {
    use aes::cipher::KeyIvInit;
    use cbc::cipher::BlockDecryptMut;

    if data.len() % 16 != 0 {
        return Err(eyre::eyre!("data not aligned to 16 bytes"));
    }

    let iv_arr: [u8; 16] = iv
        .try_into()
        .map_err(|_| eyre::eyre!("IV must be 16 bytes"))?;
    let decryptor = cbc::Decryptor::<aes::Aes256>::new(key.into(), &iv_arr.into());
    decryptor
        .decrypt_padded_mut::<cbc::cipher::block_padding::NoPadding>(data)
        .map_err(|e| eyre::eyre!("AES-256-CBC decryption failed: {e}"))?;
    Ok(())
}

/// SHA-256 signature verification for Lark webhook events.
fn verify_signature(timestamp: &str, nonce: &str, encrypt_key: &str, body: &str) -> String {
    let content = format!("{timestamp}{nonce}{encrypt_key}{body}");
    let hash = sha256(content.as_bytes());
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

pub struct FeishuChannel {
    app_id: String,
    app_secret: String,
    base_url: String,
    /// Domain root (no /open-apis path) for the WS gateway endpoint.
    domain: String,
    allowed_senders: HashSet<String>,
    shutdown: Arc<AtomicBool>,
    http: Client,
    media_dir: PathBuf,
    token_cache: Arc<tokio::sync::Mutex<Option<(String, Instant)>>>,
    dedup: MessageDedup,
    /// "ws" for WebSocket long connection, "webhook" for HTTP webhook mode.
    mode: String,
    /// Port for webhook HTTP server (only used in webhook mode).
    webhook_port: u16,
    /// Optional encrypt key for AES-256-CBC decryption.
    encrypt_key: Option<String>,
    /// Optional verification token for event validation.
    verification_token: Option<String>,
}

impl FeishuChannel {
    pub fn new(
        app_id: &str,
        app_secret: &str,
        allowed_senders: Vec<String>,
        shutdown: Arc<AtomicBool>,
        region: &str,
        media_dir: PathBuf,
    ) -> Self {
        Self {
            app_id: app_id.to_string(),
            app_secret: app_secret.to_string(),
            base_url: base_url_for_region(region),
            domain: domain_for_region(region),
            allowed_senders: allowed_senders.into_iter().collect(),
            shutdown,
            http: Client::new(),
            media_dir,
            token_cache: Arc::new(tokio::sync::Mutex::new(None)),
            dedup: MessageDedup::new(),
            mode: "ws".to_string(),
            webhook_port: 9321,
            encrypt_key: None,
            verification_token: None,
        }
    }

    /// Set mode: "ws" for WebSocket, "webhook" for HTTP webhook.
    pub fn with_mode(mut self, mode: &str) -> Self {
        self.mode = mode.to_string();
        self
    }

    /// Set webhook port (default 9321).
    pub fn with_webhook_port(mut self, port: u16) -> Self {
        self.webhook_port = port;
        self
    }

    /// Set encrypt key for AES-256-CBC event decryption.
    pub fn with_encrypt_key(mut self, key: Option<String>) -> Self {
        self.encrypt_key = key;
        self
    }

    /// Set verification token for event validation.
    pub fn with_verification_token(mut self, token: Option<String>) -> Self {
        self.verification_token = token;
        self
    }

    fn check_allowed(&self, sender_id: &str) -> bool {
        self.allowed_senders.is_empty() || self.allowed_senders.contains(sender_id)
    }

    /// Build a TLS connector for the WebSocket connection.
    fn make_tls_connector() -> Result<tokio_tungstenite::Connector> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut root_store = rustls::RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs().certs {
            root_store.add(cert).ok();
        }
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .wrap_err("Feishu: failed to configure TLS")?
            .with_root_certificates(root_store)
            .with_no_client_auth();
        Ok(tokio_tungstenite::Connector::Rustls(Arc::new(config)))
    }

    /// Get or refresh tenant access token.
    async fn get_token(&self) -> Result<String> {
        let mut cache = self.token_cache.lock().await;
        if let Some((ref token, ref created)) = *cache {
            if created.elapsed().as_secs() < TOKEN_TTL_SECS {
                return Ok(token.clone());
            }
        }

        let http_resp = self
            .http
            .post(format!(
                "{}/auth/v3/tenant_access_token/internal",
                self.base_url
            ))
            .json(&serde_json::json!({
                "app_id": self.app_id,
                "app_secret": self.app_secret,
            }))
            .send()
            .await
            .wrap_err("failed to get tenant token")?;

        let status = http_resp.status();
        let body = http_resp
            .text()
            .await
            .wrap_err("failed to read token response body")?;

        if !status.is_success() {
            return Err(eyre::eyre!(
                "Feishu token request failed (HTTP {status}): {body}"
            ));
        }

        let resp: serde_json::Value = serde_json::from_str(&body)
            .wrap_err_with(|| format!("Feishu token response is not JSON: {body}"))?;

        let token = resp
            .get("tenant_access_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                let msg = resp
                    .get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                eyre::eyre!("Feishu token error: {msg}")
            })?
            .to_string();

        *cache = Some((token.clone(), Instant::now()));
        Ok(token)
    }

    /// Get WebSocket gateway URL from Feishu bot gateway endpoint.
    /// Uses the domain root (not /open-apis) matching the official SDK behaviour.
    async fn get_ws_url(&self) -> Result<String> {
        let http_resp = self
            .http
            .post(format!("{}/callback/ws/endpoint", self.domain))
            .json(&serde_json::json!({
                "AppID": self.app_id,
                "AppSecret": self.app_secret,
            }))
            .send()
            .await
            .wrap_err("failed to get Feishu WS endpoint")?;

        let status = http_resp.status();
        let body = http_resp
            .text()
            .await
            .wrap_err("failed to read WS endpoint response body")?;

        if !status.is_success() {
            return Err(eyre::eyre!(
                "Feishu WS endpoint request failed (HTTP {status}): {body}"
            ));
        }

        let resp: serde_json::Value = serde_json::from_str(&body)
            .wrap_err_with(|| format!("Feishu WS endpoint response is not JSON: {body}"))?;

        let data = resp.get("data").ok_or_else(|| {
            let msg = resp
                .get("msg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            eyre::eyre!("Feishu WS endpoint error: {msg}")
        })?;

        let url = data
            .get("URL")
            .or_else(|| data.get("url"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre::eyre!("no URL in Feishu WS endpoint response: {data}"))?;

        if url.is_empty() {
            return Err(eyre::eyre!(
                "Feishu WS endpoint returned empty URL, full response: {resp}"
            ));
        }

        info!(url, "Feishu: got WS gateway URL");
        Ok(url.to_string())
    }

    /// Download a media resource from a Feishu message.
    async fn download_feishu_media(
        &self,
        message_id: &str,
        file_key: &str,
        resource_type: &str,
        ext: &str,
    ) -> Result<PathBuf> {
        let token = self.get_token().await?;
        let url = format!(
            "{}/im/v1/messages/{}/resources/{}?type={}",
            self.base_url, message_id, file_key, resource_type
        );
        let filename = format!("feishu_{}{}", Utc::now().timestamp_millis(), ext);
        download_media(
            &self.http,
            &url,
            &[("Authorization", &format!("Bearer {token}"))],
            &self.media_dir,
            &filename,
        )
        .await
    }

    /// Upload an image and return the image_key.
    async fn upload_image(&self, file_path: &str) -> Result<String> {
        let token = self.get_token().await?;
        let data = std::fs::read(file_path).wrap_err("failed to read image file")?;
        let filename = std::path::Path::new(file_path)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "image.png".to_string());

        let part = reqwest::multipart::Part::bytes(data)
            .file_name(filename)
            .mime_str("application/octet-stream")?;
        let form = reqwest::multipart::Form::new()
            .text("image_type", "message")
            .part("image", part);

        let resp: serde_json::Value = self
            .http
            .post(format!("{}/im/v1/images", self.base_url))
            .header("Authorization", format!("Bearer {token}"))
            .multipart(form)
            .send()
            .await
            .wrap_err("failed to upload image to Feishu")?
            .json()
            .await?;

        resp.get("data")
            .and_then(|d| d.get("image_key"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| {
                let msg = resp
                    .get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                eyre::eyre!("Feishu image upload error: {msg}")
            })
    }

    /// Upload a file and return the file_key.
    async fn upload_file(&self, file_path: &str) -> Result<String> {
        let token = self.get_token().await?;
        let data = std::fs::read(file_path).wrap_err("failed to read file")?;
        let filename = std::path::Path::new(file_path)
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".to_string());

        let part = reqwest::multipart::Part::bytes(data)
            .file_name(filename.clone())
            .mime_str("application/octet-stream")?;
        let form = reqwest::multipart::Form::new()
            .text("file_type", "stream")
            .text("file_name", filename)
            .part("file", part);

        let resp: serde_json::Value = self
            .http
            .post(format!("{}/im/v1/files", self.base_url))
            .header("Authorization", format!("Bearer {token}"))
            .multipart(form)
            .send()
            .await
            .wrap_err("failed to upload file to Feishu")?
            .json()
            .await?;

        resp.get("data")
            .and_then(|d| d.get("file_key"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| {
                let msg = resp
                    .get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                eyre::eyre!("Feishu file upload error: {msg}")
            })
    }

    /// Send a typed message via Feishu REST API.
    async fn send_message(&self, chat_id: &str, msg_type: &str, content: &str) -> Result<()> {
        self.send_message_returning_id(chat_id, msg_type, content)
            .await?;
        Ok(())
    }

    /// Send a message and return its message_id from the API response.
    async fn send_message_returning_id(
        &self,
        chat_id: &str,
        msg_type: &str,
        content: &str,
    ) -> Result<Option<String>> {
        let token = self.get_token().await?;
        let id_type = Self::receive_id_type(chat_id);

        let body = serde_json::json!({
            "receive_id": chat_id,
            "msg_type": msg_type,
            "content": content,
        });

        let resp: serde_json::Value = self
            .http
            .post(format!(
                "{}/im/v1/messages?receive_id_type={id_type}",
                self.base_url
            ))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .wrap_err("failed to send Feishu message")?
            .json()
            .await?;

        let code = resp.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            let err_msg = resp
                .get("msg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            warn!("Feishu send error: {err_msg}");
            return Ok(None);
        }

        // Extract message_id from response: { "data": { "message_id": "om_..." } }
        let message_id = resp
            .get("data")
            .and_then(|d| d.get("message_id"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        Ok(message_id)
    }

    /// Determine receive_id_type from chat_id prefix.
    fn receive_id_type(chat_id: &str) -> &'static str {
        if chat_id.starts_with("oc_") {
            "chat_id"
        } else {
            "open_id"
        }
    }

    /// Parse event JSON (shared between WS and webhook modes).
    /// Returns Some(InboundMessage) if the event is valid and should be dispatched.
    async fn parse_event(&self, envelope: &serde_json::Value) -> Option<InboundMessage> {
        let event_type = envelope
            .get("header")
            .and_then(|h| h.get("event_type"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if event_type != "im.message.receive_v1" {
            debug!(event_type, "Feishu: ignoring non-message event");
            return None;
        }

        let event = envelope.get("event")?;
        let message = event.get("message")?;

        let message_id = message
            .get("message_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if message_id.is_empty() || self.dedup.is_duplicate(message_id) {
            debug!(message_id, "Feishu: dedup filtered message");
            return None;
        }

        let sender_id = event
            .get("sender")
            .and_then(|s| s.get("sender_id"))
            .and_then(|s| s.get("open_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let chat_id = message
            .get("chat_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if sender_id.is_empty() || chat_id.is_empty() {
            return None;
        }

        if !self.check_allowed(sender_id) {
            return None;
        }

        let msg_type = message
            .get("message_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let content_json: Option<serde_json::Value> = message
            .get("content")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_json::from_str(s).ok());

        let mut content = String::new();
        let mut media = Vec::new();

        match msg_type {
            "text" => {
                content = content_json
                    .as_ref()
                    .and_then(|v| v.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string();
            }
            "image" => {
                if let Some(key) = content_json
                    .as_ref()
                    .and_then(|v| v.get("image_key"))
                    .and_then(|v| v.as_str())
                {
                    match self
                        .download_feishu_media(message_id, key, "image", ".png")
                        .await
                    {
                        Ok(path) => media.push(path.display().to_string()),
                        Err(e) => warn!("failed to download Feishu image: {e}"),
                    }
                }
            }
            "file" => {
                let file_key = content_json
                    .as_ref()
                    .and_then(|v| v.get("file_key"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let file_name = content_json
                    .as_ref()
                    .and_then(|v| v.get("file_name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !file_key.is_empty() {
                    let ext = std::path::Path::new(file_name)
                        .extension()
                        .map(|e| format!(".{}", e.to_string_lossy()))
                        .unwrap_or_default();
                    match self
                        .download_feishu_media(message_id, file_key, "file", &ext)
                        .await
                    {
                        Ok(path) => media.push(path.display().to_string()),
                        Err(e) => warn!("failed to download Feishu file: {e}"),
                    }
                }
            }
            "audio" => {
                if let Some(key) = content_json
                    .as_ref()
                    .and_then(|v| v.get("file_key"))
                    .and_then(|v| v.as_str())
                {
                    match self
                        .download_feishu_media(message_id, key, "file", ".ogg")
                        .await
                    {
                        Ok(path) => media.push(path.display().to_string()),
                        Err(e) => warn!("failed to download Feishu audio: {e}"),
                    }
                }
            }
            "media" => {
                if let Some(key) = content_json
                    .as_ref()
                    .and_then(|v| v.get("file_key"))
                    .and_then(|v| v.as_str())
                {
                    match self
                        .download_feishu_media(message_id, key, "file", ".mp4")
                        .await
                    {
                        Ok(path) => media.push(path.display().to_string()),
                        Err(e) => warn!("failed to download Feishu video: {e}"),
                    }
                }
            }
            "sticker" => {
                if let Some(key) = content_json
                    .as_ref()
                    .and_then(|v| v.get("file_key"))
                    .and_then(|v| v.as_str())
                {
                    match self
                        .download_feishu_media(message_id, key, "file", ".png")
                        .await
                    {
                        Ok(path) => media.push(path.display().to_string()),
                        Err(e) => warn!("failed to download Feishu sticker: {e}"),
                    }
                }
            }
            _ => {
                content = format!("[{msg_type} message]");
            }
        }

        if content.is_empty() && media.is_empty() {
            debug!(
                message_id,
                msg_type, "Feishu: empty content and media, skipping"
            );
            return None;
        }

        info!(
            message_id,
            msg_type,
            media_count = media.len(),
            "Feishu: parsed event"
        );

        Some(InboundMessage {
            channel: "feishu".into(),
            sender_id: sender_id.to_string(),
            chat_id: chat_id.to_string(),
            content,
            timestamp: Utc::now(),
            media,
            metadata: serde_json::json!({
                "feishu": {
                    "message_id": message_id,
                    "message_type": msg_type,
                }
            }),
            message_id: None,
        })
    }

    /// Run WebSocket long connection mode using the Feishu binary frame protocol.
    async fn start_ws(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        loop {
            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }

            let ws_url = match self.get_ws_url().await {
                Ok(url) => url,
                Err(e) => {
                    error!("Failed to get Feishu WS URL: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };

            // Extract service_id from the WS URL query params.
            let service_id: i32 = ws_url
                .split('?')
                .nth(1)
                .unwrap_or("")
                .split('&')
                .find_map(|pair| {
                    let (k, v) = pair.split_once('=')?;
                    if k == "service_id" {
                        v.parse().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);

            let connector = match Self::make_tls_connector() {
                Ok(c) => c,
                Err(e) => {
                    error!("Feishu: failed to create TLS connector: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };
            let (ws_stream, _) = match tokio_tungstenite::connect_async_tls_with_config(
                &ws_url,
                None,
                false,
                Some(connector),
            )
            .await
            {
                Ok(conn) => conn,
                Err(e) => {
                    error!("Failed to connect Feishu WebSocket: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    continue;
                }
            };

            info!("Feishu WebSocket connected");
            let (mut ws_tx, mut ws_rx) = ws_stream.split();

            // Spawn a ping loop task.
            let shutdown_ping = self.shutdown.clone();
            let ping_handle = tokio::spawn({
                let (ping_tx, mut ping_rx) = mpsc::channel::<Vec<u8>>(4);

                // Producer: generate pings on interval.
                let ping_producer = tokio::spawn(async move {
                    let mut interval = tokio::time::interval(std::time::Duration::from_secs(
                        FEISHU_PING_INTERVAL_SECS,
                    ));
                    loop {
                        interval.tick().await;
                        if shutdown_ping.load(Ordering::Relaxed) {
                            break;
                        }
                        let ping = new_ping_frame(service_id).encode();
                        if ping_tx.send(ping).await.is_err() {
                            break;
                        }
                    }
                });

                // Consumer: forward pings to the WS sink.
                async move {
                    while let Some(data) = ping_rx.recv().await {
                        if ws_tx.send(WsMessage::Binary(data.into())).await.is_err() {
                            break;
                        }
                    }
                    // Return the sink so we can close it.
                    ping_producer.abort();
                    ws_tx
                }
            });

            while let Some(msg) = ws_rx.next().await {
                if self.shutdown.load(Ordering::Relaxed) {
                    break;
                }

                let binary = match msg {
                    Ok(WsMessage::Binary(bin)) => bin,
                    Ok(WsMessage::Close(_)) => {
                        info!("Feishu WebSocket closed by server");
                        break;
                    }
                    Ok(WsMessage::Text(text)) => {
                        debug!(len = text.len(), "Feishu: unexpected text frame");
                        continue;
                    }
                    Ok(_) => continue,
                    Err(e) => {
                        warn!("Feishu WebSocket error: {e}");
                        break;
                    }
                };

                let frame = match Frame::decode(&binary) {
                    Ok(f) => f,
                    Err(e) => {
                        debug!("Feishu: failed to decode frame: {e}");
                        continue;
                    }
                };

                let msg_type = frame.get_header(HEADER_TYPE).unwrap_or("");
                debug!(
                    method = frame.method,
                    msg_type,
                    payload_len = frame.payload.len(),
                    "Feishu: received frame"
                );

                if frame.method == FRAME_TYPE_CONTROL {
                    // Control frame: ping/pong.
                    if msg_type == MSG_TYPE_PONG {
                        debug!("Feishu: received pong");
                    }
                    continue;
                }

                if frame.method == FRAME_TYPE_DATA {
                    // Data frame: event or card callback.
                    if msg_type != MSG_TYPE_EVENT {
                        debug!(msg_type, "Feishu: ignoring non-event data frame");
                        continue;
                    }

                    // Handle multi-part message assembly.
                    let sum = frame.get_header_int(HEADER_SUM);
                    if sum > 1 {
                        debug!(
                            sum,
                            seq = frame.get_header_int(HEADER_SEQ),
                            "Feishu: multi-part frame (not yet supported), skipping"
                        );
                        continue;
                    }

                    let payload_str = match std::str::from_utf8(&frame.payload) {
                        Ok(s) => s,
                        Err(e) => {
                            debug!("Feishu: payload is not UTF-8: {e}");
                            continue;
                        }
                    };

                    let envelope: serde_json::Value = match serde_json::from_str(payload_str) {
                        Ok(v) => v,
                        Err(e) => {
                            debug!("Feishu: failed to parse event payload: {e}");
                            continue;
                        }
                    };

                    if let Some(inbound) = self.parse_event(&envelope).await {
                        if inbound_tx.send(inbound).await.is_err() {
                            ping_handle.abort();
                            return Ok(());
                        }
                    }
                }
            }

            ping_handle.abort();

            if self.shutdown.load(Ordering::Relaxed) {
                break;
            }

            warn!("Feishu WebSocket disconnected, reconnecting in 2s...");
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }

        Ok(())
    }

    /// Run webhook HTTP server mode.
    async fn start_webhook(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        use axum::{
            Router, extract::State, http::HeaderMap, response::IntoResponse, routing::post,
        };

        #[derive(Clone)]
        struct WebhookState {
            encrypt_key: Option<String>,
            verification_token: Option<String>,
            inbound_tx: mpsc::Sender<serde_json::Value>,
        }

        async fn handle_webhook(
            State(state): State<WebhookState>,
            headers: HeaderMap,
            body: String,
        ) -> impl IntoResponse {
            // Try to parse the body as JSON
            let body_json: serde_json::Value = match serde_json::from_str(&body) {
                Ok(v) => v,
                Err(e) => {
                    warn!("Feishu webhook: invalid JSON body: {e}");
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        axum::Json(serde_json::json!({"error": "invalid json"})),
                    )
                        .into_response();
                }
            };

            // Signature verification if encrypt_key is set
            if let Some(ref ek) = state.encrypt_key {
                let timestamp = headers
                    .get("X-Lark-Request-Timestamp")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let nonce = headers
                    .get("X-Lark-Request-Nonce")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let expected_sig = headers
                    .get("X-Lark-Signature")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");

                if !timestamp.is_empty() && !nonce.is_empty() && !expected_sig.is_empty() {
                    let computed = verify_signature(timestamp, nonce, ek, &body);
                    if computed != expected_sig {
                        warn!("Feishu webhook: signature mismatch");
                        return (
                            axum::http::StatusCode::FORBIDDEN,
                            axum::Json(serde_json::json!({"error": "signature mismatch"})),
                        )
                            .into_response();
                    }
                }
            }

            // Decrypt if encrypted
            let event_json = if let Some(encrypt_str) =
                body_json.get("encrypt").and_then(|v| v.as_str())
            {
                if let Some(ref ek) = state.encrypt_key {
                    match decrypt_lark_event(ek, encrypt_str) {
                        Ok(decrypted) => match serde_json::from_str(&decrypted) {
                            Ok(v) => v,
                            Err(e) => {
                                warn!("Feishu webhook: failed to parse decrypted event: {e}");
                                return (
                                    axum::http::StatusCode::BAD_REQUEST,
                                    axum::Json(serde_json::json!({"error": "decrypt parse error"})),
                                )
                                    .into_response();
                            }
                        },
                        Err(e) => {
                            warn!("Feishu webhook: decryption failed: {e}");
                            return (
                                axum::http::StatusCode::BAD_REQUEST,
                                axum::Json(serde_json::json!({"error": "decryption failed"})),
                            )
                                .into_response();
                        }
                    }
                } else {
                    warn!("Feishu webhook: received encrypted event but no encrypt_key configured");
                    return (
                        axum::http::StatusCode::BAD_REQUEST,
                        axum::Json(serde_json::json!({"error": "no encrypt key configured"})),
                    )
                        .into_response();
                }
            } else {
                body_json
            };

            // Handle url_verification challenge
            if event_json.get("type").and_then(|v| v.as_str()) == Some("url_verification") {
                let challenge = event_json
                    .get("challenge")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                info!("Feishu webhook: url_verification challenge received");
                return axum::Json(serde_json::json!({"challenge": challenge})).into_response();
            }

            // Verification token check (for non-encrypted plaintext events)
            if let Some(ref vt) = state.verification_token {
                let event_token = event_json
                    .get("token")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if !event_token.is_empty() && event_token != vt {
                    warn!("Feishu webhook: verification token mismatch");
                    return (
                        axum::http::StatusCode::FORBIDDEN,
                        axum::Json(serde_json::json!({"error": "token mismatch"})),
                    )
                        .into_response();
                }
            }

            // Forward event to the channel for processing
            let _ = state.inbound_tx.send(event_json).await;

            "ok".into_response()
        }

        // Internal channel for passing parsed events
        let (event_tx, mut event_rx) = mpsc::channel::<serde_json::Value>(100);

        let state = WebhookState {
            encrypt_key: self.encrypt_key.clone(),
            verification_token: self.verification_token.clone(),
            inbound_tx: event_tx,
        };

        let app = Router::new()
            .route("/webhook/event", post(handle_webhook))
            .with_state(state);

        let addr = format!("0.0.0.0:{}", self.webhook_port);
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .wrap_err_with(|| format!("failed to bind webhook server to {addr}"))?;
        info!(port = self.webhook_port, "Feishu webhook server listening");

        let shutdown = self.shutdown.clone();

        // Spawn the HTTP server
        let server_shutdown = shutdown.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    while !server_shutdown.load(Ordering::Relaxed) {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    }
                })
                .await
                .ok();
        });

        // Process incoming events
        while let Some(envelope) = event_rx.recv().await {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }

            if let Some(inbound) = self.parse_event(&envelope).await {
                info!(sender = %inbound.sender_id, chat = %inbound.chat_id, "Feishu: sending to inbound bus");
                if inbound_tx.send(inbound).await.is_err() {
                    error!("Feishu: inbound_tx send failed (receiver dropped)");
                    break;
                }
            }
        }

        Ok(())
    }
}

#[async_trait]
impl Channel for FeishuChannel {
    fn name(&self) -> &str {
        "feishu"
    }

    fn supports_edit(&self) -> bool {
        true
    }

    async fn start(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        info!(base_url = %self.base_url, mode = %self.mode, "Starting Feishu/Lark channel");

        match self.mode.as_str() {
            "webhook" => self.start_webhook(inbound_tx).await?,
            _ => self.start_ws(inbound_tx).await?,
        }

        info!("Feishu channel stopped");
        Ok(())
    }

    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        // Send text content as interactive card with markdown
        if !msg.content.is_empty() {
            let card = serde_json::json!({
                "elements": [
                    {
                        "tag": "markdown",
                        "content": msg.content,
                    }
                ]
            });
            self.send_message(&msg.chat_id, "interactive", &card.to_string())
                .await?;
        }

        // Send media files
        for path in &msg.media {
            if is_image(path) {
                match self.upload_image(path).await {
                    Ok(image_key) => {
                        let content = serde_json::json!({"image_key": image_key}).to_string();
                        self.send_message(&msg.chat_id, "image", &content).await?;
                    }
                    Err(e) => warn!("failed to upload Feishu image: {e}"),
                }
            } else {
                match self.upload_file(path).await {
                    Ok(file_key) => {
                        let filename = std::path::Path::new(path)
                            .file_name()
                            .map(|f| f.to_string_lossy().to_string())
                            .unwrap_or_else(|| "file".to_string());
                        let content =
                            serde_json::json!({"file_key": file_key, "file_name": filename})
                                .to_string();
                        self.send_message(&msg.chat_id, "file", &content).await?;
                    }
                    Err(e) => warn!("failed to upload Feishu file: {e}"),
                }
            }
        }

        Ok(())
    }

    fn is_allowed(&self, sender_id: &str) -> bool {
        self.check_allowed(sender_id)
    }

    async fn stop(&self) -> Result<()> {
        self.shutdown.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn send_with_id(&self, msg: &OutboundMessage) -> Result<Option<String>> {
        if msg.content.is_empty() {
            return Ok(None);
        }
        let card = serde_json::json!({
            "elements": [
                {
                    "tag": "markdown",
                    "content": msg.content,
                }
            ]
        });
        self.send_message_returning_id(&msg.chat_id, "interactive", &card.to_string())
            .await
    }

    async fn edit_message(
        &self,
        _chat_id: &str,
        message_id: &str,
        new_content: &str,
    ) -> Result<()> {
        let token = self.get_token().await?;
        let card = serde_json::json!({
            "elements": [
                {
                    "tag": "markdown",
                    "content": new_content,
                }
            ]
        });
        let body = serde_json::json!({
            "msg_type": "interactive",
            "content": card.to_string(),
        });

        let resp: serde_json::Value = self
            .http
            .patch(format!("{}/im/v1/messages/{}", self.base_url, message_id))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .wrap_err("failed to edit Feishu message")?
            .json()
            .await?;

        let code = resp.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            let err_msg = resp
                .get("msg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            warn!("Feishu edit error: {err_msg}");
        }
        Ok(())
    }

    async fn delete_message(&self, _chat_id: &str, message_id: &str) -> Result<()> {
        let token = self.get_token().await?;

        let resp: serde_json::Value = self
            .http
            .delete(format!("{}/im/v1/messages/{}", self.base_url, message_id))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .wrap_err("failed to delete Feishu message")?
            .json()
            .await?;

        let code = resp.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
        if code != 0 {
            let err_msg = resp
                .get("msg")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            warn!("Feishu delete error: {err_msg}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_channel(allowed: Vec<&str>) -> FeishuChannel {
        make_channel_with_region(allowed, "cn")
    }

    fn make_channel_with_region(allowed: Vec<&str>, region: &str) -> FeishuChannel {
        FeishuChannel {
            app_id: "test_id".into(),
            app_secret: "test_secret".into(),
            base_url: base_url_for_region(region),
            domain: domain_for_region(region),
            allowed_senders: allowed.into_iter().map(String::from).collect(),
            shutdown: Arc::new(AtomicBool::new(false)),
            http: Client::new(),
            media_dir: PathBuf::from("/tmp/test-feishu-media"),
            token_cache: Arc::new(tokio::sync::Mutex::new(None)),
            dedup: MessageDedup::new(),
            mode: "ws".into(),
            webhook_port: 9321,
            encrypt_key: None,
            verification_token: None,
        }
    }

    #[test]
    fn test_base_url_cn() {
        let ch = make_channel_with_region(vec![], "cn");
        assert_eq!(ch.base_url, "https://open.feishu.cn/open-apis");
    }

    #[test]
    fn test_base_url_global() {
        let ch = make_channel_with_region(vec![], "global");
        assert_eq!(ch.base_url, "https://open.larksuite.com/open-apis");
    }

    #[test]
    fn test_base_url_lark_alias() {
        let ch = make_channel_with_region(vec![], "lark");
        assert_eq!(ch.base_url, "https://open.larksuite.com/open-apis");
    }

    #[test]
    fn test_base_url_default_cn() {
        let ch = make_channel_with_region(vec![], "anything_else");
        assert_eq!(ch.base_url, "https://open.feishu.cn/open-apis");
    }

    #[test]
    fn test_is_allowed_empty_list() {
        let ch = make_channel(vec![]);
        assert!(ch.is_allowed("anyone"));
    }

    #[test]
    fn test_is_allowed_matching() {
        let ch = make_channel(vec!["ou_123", "ou_456"]);
        assert!(ch.is_allowed("ou_123"));
        assert!(!ch.is_allowed("ou_789"));
    }

    #[test]
    fn test_receive_id_type() {
        assert_eq!(FeishuChannel::receive_id_type("oc_abc123"), "chat_id");
        assert_eq!(FeishuChannel::receive_id_type("ou_xyz789"), "open_id");
        assert_eq!(FeishuChannel::receive_id_type("other"), "open_id");
    }

    #[test]
    fn test_dedup() {
        let ch = make_channel(vec![]);
        assert!(!ch.dedup.is_duplicate("msg1"));
        assert!(ch.dedup.is_duplicate("msg1")); // duplicate
        assert!(!ch.dedup.is_duplicate("msg2"));
    }

    #[test]
    fn test_message_content_text() {
        let content_str = r#"{"text":"Hello world"}"#;
        let parsed: serde_json::Value = serde_json::from_str(content_str).unwrap();
        let text = parsed.get("text").and_then(|t| t.as_str()).unwrap();
        assert_eq!(text, "Hello world");
    }

    #[test]
    fn test_message_content_image() {
        let content_str = r#"{"image_key":"img_abc123"}"#;
        let parsed: serde_json::Value = serde_json::from_str(content_str).unwrap();
        let key = parsed.get("image_key").and_then(|t| t.as_str()).unwrap();
        assert_eq!(key, "img_abc123");
    }

    #[test]
    fn test_message_content_file() {
        let content_str = r#"{"file_key":"file_xyz","file_name":"report.pdf"}"#;
        let parsed: serde_json::Value = serde_json::from_str(content_str).unwrap();
        let key = parsed.get("file_key").and_then(|t| t.as_str()).unwrap();
        let name = parsed.get("file_name").and_then(|t| t.as_str()).unwrap();
        assert_eq!(key, "file_xyz");
        assert_eq!(name, "report.pdf");
    }

    #[test]
    fn test_sha256_basic() {
        let hash = sha256(b"test key");
        // Known SHA-256 of "test key"
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "fa2bdca424f01f01ffb48df93acc35d439c7fd331a1a7fba6ac2fd83aa9ab31a"
        );
    }

    #[test]
    fn test_base64_decode() {
        let decoded = base64_decode("aGVsbG8gd29ybGQ=").unwrap();
        assert_eq!(decoded, b"hello world");
    }

    #[test]
    fn test_decrypt_lark_event() {
        // Official test vector from Lark docs: encrypt key="test key", plaintext="hello world"
        let result =
            decrypt_lark_event("test key", "P37w+VZImNgPEO1RBhJ6RtKl7n6zymIbEG1pReEzghk=").unwrap();
        assert_eq!(result, "hello world");
    }

    #[test]
    fn test_verify_signature() {
        let sig = verify_signature("ts123", "nonce456", "mykey", r#"{"test":"body"}"#);
        // Should be a 64-char hex string
        assert_eq!(sig.len(), 64);
        // Deterministic
        let sig2 = verify_signature("ts123", "nonce456", "mykey", r#"{"test":"body"}"#);
        assert_eq!(sig, sig2);
    }

    #[test]
    fn test_with_mode() {
        let ch = FeishuChannel::new(
            "id",
            "secret",
            vec![],
            Arc::new(AtomicBool::new(false)),
            "global",
            PathBuf::from("/tmp"),
        )
        .with_mode("webhook")
        .with_webhook_port(8080);
        assert_eq!(ch.mode, "webhook");
        assert_eq!(ch.webhook_port, 8080);
    }
}
