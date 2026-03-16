//! Twilio channel for SMS/MMS/RCS/WhatsApp Business messaging.
//!
//! Receives inbound messages via Twilio webhooks (HTTP POST) and sends
//! outbound messages via the Twilio REST API.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use chrono::Utc;
use octos_core::{InboundMessage, OutboundMessage};
use eyre::{Result, WrapErr};
use reqwest::Client;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::channel::Channel;
use crate::media::download_media;

/// Maximum message IDs to track for dedup.
const MAX_SEEN_IDS: usize = 1000;

pub struct TwilioChannel {
    account_sid: String,
    auth_token: String,
    from_number: String,
    webhook_port: u16,
    allowed_senders: HashSet<String>,
    shutdown: Arc<AtomicBool>,
    http: Client,
    media_dir: PathBuf,
    seen_ids: Arc<std::sync::Mutex<HashSet<String>>>,
}

impl TwilioChannel {
    pub fn new(
        account_sid: &str,
        auth_token: &str,
        from_number: &str,
        allowed_senders: Vec<String>,
        shutdown: Arc<AtomicBool>,
        media_dir: PathBuf,
        webhook_port: u16,
    ) -> Self {
        Self {
            account_sid: account_sid.to_string(),
            auth_token: auth_token.to_string(),
            from_number: from_number.to_string(),
            webhook_port,
            allowed_senders: allowed_senders.into_iter().collect(),
            shutdown,
            http: Client::new(),
            media_dir,
            seen_ids: Arc::new(std::sync::Mutex::new(HashSet::new())),
        }
    }

    fn check_allowed(&self, sender_id: &str) -> bool {
        self.allowed_senders.is_empty() || self.allowed_senders.contains(sender_id)
    }

    /// Clean a phone number for use as sender_id (strip "whatsapp:" prefix).
    fn clean_number(number: &str) -> &str {
        number.strip_prefix("whatsapp:").unwrap_or(number)
    }
}

/// Verify Twilio request signature (HMAC-SHA1).
///
/// Algorithm: HMAC-SHA1(auth_token, url + sorted_post_params) == X-Twilio-Signature
fn verify_twilio_signature(
    auth_token: &str,
    url: &str,
    params: &[(String, String)],
    signature: &str,
) -> bool {
    let mut sorted_params = params.to_vec();
    sorted_params.sort_by(|a, b| a.0.cmp(&b.0));

    let mut data = url.to_string();
    for (key, value) in &sorted_params {
        data.push_str(key);
        data.push_str(value);
    }

    let computed = hmac_sha1(auth_token.as_bytes(), data.as_bytes());
    let computed_b64 = base64_encode(&computed);

    computed_b64 == signature
}

/// HMAC-SHA1 implementation.
fn hmac_sha1(key: &[u8], message: &[u8]) -> [u8; 20] {
    const BLOCK_SIZE: usize = 64;

    // If key is longer than block size, hash it
    let key = if key.len() > BLOCK_SIZE {
        let h = sha1(key);
        h.to_vec()
    } else {
        key.to_vec()
    };

    // Pad key to block size
    let mut ipad = vec![0x36u8; BLOCK_SIZE];
    let mut opad = vec![0x5cu8; BLOCK_SIZE];
    for (i, &b) in key.iter().enumerate() {
        ipad[i] ^= b;
        opad[i] ^= b;
    }

    // Inner hash: SHA1(ipad || message)
    let mut inner = ipad;
    inner.extend_from_slice(message);
    let inner_hash = sha1(&inner);

    // Outer hash: SHA1(opad || inner_hash)
    let mut outer = opad;
    outer.extend_from_slice(&inner_hash);
    sha1(&outer)
}

/// Minimal SHA-1 implementation.
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h0: u32 = 0x67452301;
    let mut h1: u32 = 0xEFCDAB89;
    let mut h2: u32 = 0x98BADCFE;
    let mut h3: u32 = 0x10325476;
    let mut h4: u32 = 0xC3D2E1F0;

    let bit_len = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let (mut a, mut b, mut c, mut d, mut e) = (h0, h1, h2, h3, h4);

        for i in 0..80 {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A827999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9EBA1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1BBCDCu32),
                _ => (b ^ c ^ d, 0xCA62C1D6u32),
            };

            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(w[i]);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    let mut result = [0u8; 20];
    result[0..4].copy_from_slice(&h0.to_be_bytes());
    result[4..8].copy_from_slice(&h1.to_be_bytes());
    result[8..12].copy_from_slice(&h2.to_be_bytes());
    result[12..16].copy_from_slice(&h3.to_be_bytes());
    result[16..20].copy_from_slice(&h4.to_be_bytes());
    result
}

/// Base64 encode bytes.
fn base64_encode(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };

        let triple = (b0 << 16) | (b1 << 8) | b2;

        out.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);

        if chunk.len() > 1 {
            out.push(TABLE[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }

        if chunk.len() > 2 {
            out.push(TABLE[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Map Twilio content type to file extension.
fn ext_from_content_type(ct: &str) -> &str {
    match ct {
        "image/jpeg" => ".jpg",
        "image/png" => ".png",
        "image/gif" => ".gif",
        "image/webp" => ".webp",
        "audio/ogg" => ".ogg",
        "audio/mpeg" | "audio/mp3" => ".mp3",
        "audio/mp4" => ".m4a",
        "video/mp4" => ".mp4",
        "video/3gpp" => ".3gp",
        "application/pdf" => ".pdf",
        _ => "",
    }
}

#[async_trait]
impl Channel for TwilioChannel {
    fn name(&self) -> &str {
        "twilio"
    }

    fn max_message_length(&self) -> usize {
        1600
    }

    fn is_allowed(&self, sender_id: &str) -> bool {
        self.check_allowed(sender_id)
    }

    async fn start(&self, inbound_tx: mpsc::Sender<InboundMessage>) -> Result<()> {
        use axum::extract::State;
        use axum::routing::post;
        use axum::{Form, Router};

        info!(
            port = self.webhook_port,
            "Starting Twilio channel (webhook mode)"
        );

        #[derive(Clone)]
        struct WebhookState {
            auth_token: String,
            from_number: String,
            allowed_senders: HashSet<String>,
            inbound_tx: mpsc::Sender<InboundMessage>,
            http: Client,
            media_dir: PathBuf,
            seen_ids: Arc<std::sync::Mutex<HashSet<String>>>,
        }

        async fn handle_webhook(
            State(state): State<WebhookState>,
            headers: axum::http::HeaderMap,
            Form(params): Form<Vec<(String, String)>>,
        ) -> axum::http::Response<String> {
            // Extract fields from form params
            let get = |key: &str| -> String {
                params
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default()
            };

            let from = get("From");
            let body = get("Body");
            let message_sid = get("MessageSid");
            let num_media: usize = get("NumMedia").parse().unwrap_or(0);

            if from.is_empty() {
                return axum::http::Response::builder()
                    .status(400)
                    .body("missing From".to_string())
                    .unwrap();
            }

            // Verify signature if present
            if let Some(sig) = headers
                .get("X-Twilio-Signature")
                .and_then(|v| v.to_str().ok())
            {
                // We need the full URL for verification — use X-Forwarded-Proto/Host or fallback
                let host = headers
                    .get("host")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("localhost");
                let proto = headers
                    .get("x-forwarded-proto")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("http");
                let path = "/twilio/webhook";
                let url = format!("{proto}://{host}{path}");

                if !verify_twilio_signature(&state.auth_token, &url, &params, sig) {
                    warn!("Twilio webhook: signature verification failed");
                    return axum::http::Response::builder()
                        .status(403)
                        .body("signature mismatch".to_string())
                        .unwrap();
                }
            }

            // Use full From value as sender_id (preserves "whatsapp:" prefix for correct reply routing)
            let sender_id = &from;
            let clean_sender = TwilioChannel::clean_number(&from);

            // Check allowed senders (match against both raw and cleaned number)
            if !state.allowed_senders.is_empty()
                && !state.allowed_senders.contains(sender_id)
                && !state.allowed_senders.contains(clean_sender)
            {
                return axum::http::Response::builder()
                    .status(200)
                    .header("Content-Type", "text/xml")
                    .body("<Response/>".to_string())
                    .unwrap();
            }

            // Dedup by MessageSid
            if !message_sid.is_empty() {
                let mut seen = state.seen_ids.lock().unwrap_or_else(|e| e.into_inner());
                if seen.contains(&message_sid) {
                    return axum::http::Response::builder()
                        .status(200)
                        .header("Content-Type", "text/xml")
                        .body("<Response/>".to_string())
                        .unwrap();
                }
                if seen.len() >= MAX_SEEN_IDS {
                    seen.clear();
                }
                seen.insert(message_sid.clone());
            }

            // Download media attachments
            let mut media = Vec::new();
            for i in 0..num_media {
                let url = get(&format!("MediaUrl{i}"));
                let content_type = get(&format!("MediaContentType{i}"));

                if url.is_empty() {
                    continue;
                }

                let ext = ext_from_content_type(&content_type);
                let filename = format!("twilio_{}{ext}", Utc::now().timestamp_millis());

                // Twilio media URLs require Basic Auth
                let auth_header = format!(
                    "Basic {}",
                    base64_encode(format!("{}:{}", "download", state.auth_token).as_bytes())
                );

                match download_media(
                    &state.http,
                    &url,
                    &[("Authorization", &auth_header)],
                    &state.media_dir,
                    &filename,
                )
                .await
                {
                    Ok(path) => media.push(path.display().to_string()),
                    Err(e) => warn!("failed to download Twilio media: {e}"),
                }
            }

            if body.is_empty() && media.is_empty() {
                return axum::http::Response::builder()
                    .status(200)
                    .header("Content-Type", "text/xml")
                    .body("<Response/>".to_string())
                    .unwrap();
            }

            let inbound = InboundMessage {
                channel: "twilio".into(),
                sender_id: sender_id.to_string(),
                chat_id: sender_id.to_string(),
                content: body,
                timestamp: Utc::now(),
                media,
                metadata: serde_json::json!({
                    "twilio": {
                        "message_sid": message_sid,
                        "from": from,
                        "from_number": state.from_number,
                    }
                }),
                message_id: None,
            };

            let _ = state.inbound_tx.send(inbound).await;

            axum::http::Response::builder()
                .status(200)
                .header("Content-Type", "text/xml")
                .body("<Response/>".to_string())
                .unwrap()
        }

        let state = WebhookState {
            auth_token: self.auth_token.clone(),
            from_number: self.from_number.clone(),
            allowed_senders: self.allowed_senders.clone(),
            inbound_tx,
            http: self.http.clone(),
            media_dir: self.media_dir.clone(),
            seen_ids: self.seen_ids.clone(),
        };

        let app = Router::new()
            .route("/twilio/webhook", post(handle_webhook))
            .with_state(state);

        let addr = format!("0.0.0.0:{}", self.webhook_port);
        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .wrap_err_with(|| format!("failed to bind Twilio webhook server to {addr}"))?;
        info!(port = self.webhook_port, "Twilio webhook server listening");

        let shutdown = self.shutdown.clone();

        // Serve with graceful shutdown
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                loop {
                    if shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            })
            .await
            .wrap_err("Twilio webhook server error")?;

        info!("Twilio channel stopped");
        Ok(())
    }

    async fn send(&self, msg: &OutboundMessage) -> Result<()> {
        let to = if msg.chat_id.starts_with('+') || msg.chat_id.starts_with("whatsapp:") {
            msg.chat_id.clone()
        } else {
            format!("+{}", msg.chat_id)
        };

        let mut form: Vec<(String, String)> = vec![
            ("From".to_string(), self.from_number.clone()),
            ("To".to_string(), to),
            ("Body".to_string(), msg.content.clone()),
        ];

        // Attach media URLs if available (Twilio needs publicly accessible URLs)
        for (i, path) in msg.media.iter().enumerate() {
            // Only include if it looks like a URL (http/https)
            if path.starts_with("http://") || path.starts_with("https://") {
                form.push((format!("MediaUrl.{i}"), path.clone()));
            }
        }

        let url = format!(
            "https://api.twilio.com/2010-04-01/Accounts/{}/Messages.json",
            self.account_sid
        );

        let resp = self
            .http
            .post(&url)
            .basic_auth(&self.account_sid, Some(&self.auth_token))
            .form(&form)
            .send()
            .await
            .wrap_err("failed to send Twilio message")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            warn!(status = %status, body = %body, "Twilio API error");
            eyre::bail!("Twilio API error: {status} - {body}");
        }

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.shutdown.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_number() {
        assert_eq!(TwilioChannel::clean_number("+15551234567"), "+15551234567");
        assert_eq!(
            TwilioChannel::clean_number("whatsapp:+15551234567"),
            "+15551234567"
        );
    }

    #[test]
    fn test_ext_from_content_type() {
        assert_eq!(ext_from_content_type("image/jpeg"), ".jpg");
        assert_eq!(ext_from_content_type("image/png"), ".png");
        assert_eq!(ext_from_content_type("video/mp4"), ".mp4");
        assert_eq!(ext_from_content_type("audio/ogg"), ".ogg");
        assert_eq!(ext_from_content_type("application/pdf"), ".pdf");
        assert_eq!(ext_from_content_type("text/plain"), "");
    }

    #[test]
    fn test_hmac_sha1() {
        // RFC 2202 test vector 1
        let key = b"Jefe";
        let data = b"what do ya want for nothing?";
        let result = hmac_sha1(key, data);
        let expected: [u8; 20] = [
            0xef, 0xfc, 0xdf, 0x6a, 0xe5, 0xeb, 0x2f, 0xa2, 0xd2, 0x74, 0x16, 0xd5, 0xf1, 0x84,
            0xdf, 0x9c, 0x25, 0x9a, 0x7c, 0x79,
        ];
        assert_eq!(result, expected);
    }

    #[test]
    fn test_base64_encode() {
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"a"), "YQ==");
        assert_eq!(base64_encode(b"ab"), "YWI=");
        assert_eq!(base64_encode(b"abc"), "YWJj");
    }

    #[test]
    fn test_verify_signature() {
        // Known Twilio test case
        let auth_token = "12345";
        let url = "https://mycompany.com/myapp.php?foo=1&bar=2";
        let params = vec![
            ("CallSid".to_string(), "CA1234567890ABCDE".to_string()),
            ("Caller".to_string(), "+14158675310".to_string()),
            ("Digits".to_string(), "1234".to_string()),
            ("From".to_string(), "+14158675310".to_string()),
            ("To".to_string(), "+18005551212".to_string()),
        ];

        // Just verify it doesn't panic and returns a bool
        let result = verify_twilio_signature(auth_token, url, &params, "invalid");
        assert!(!result);
    }

    #[test]
    fn test_allowed_senders() {
        let ch = TwilioChannel::new(
            "ACtest",
            "token",
            "+15551234567",
            vec!["+15559876543".into()],
            Arc::new(AtomicBool::new(false)),
            PathBuf::from("/tmp"),
            8090,
        );
        assert!(ch.check_allowed("+15559876543"));
        assert!(!ch.check_allowed("+15550000000"));

        // Empty list = allow all
        let ch2 = TwilioChannel::new(
            "ACtest",
            "token",
            "+15551234567",
            vec![],
            Arc::new(AtomicBool::new(false)),
            PathBuf::from("/tmp"),
            8090,
        );
        assert!(ch2.check_allowed("+15550000000"));
    }

    #[test]
    fn test_channel_name() {
        let ch = TwilioChannel::new(
            "ACtest",
            "token",
            "+15551234567",
            vec![],
            Arc::new(AtomicBool::new(false)),
            PathBuf::from("/tmp"),
            8090,
        );
        assert_eq!(ch.name(), "twilio");
    }

    #[test]
    fn test_max_message_length() {
        let ch = TwilioChannel::new(
            "ACtest",
            "token",
            "+15551234567",
            vec![],
            Arc::new(AtomicBool::new(false)),
            PathBuf::from("/tmp"),
            8090,
        );
        assert_eq!(ch.max_message_length(), 1600);
    }

    #[test]
    fn test_sha1_empty() {
        let hash = sha1(b"");
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    }

    #[test]
    fn test_sha1_hello() {
        let hash = sha1(b"hello");
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d");
    }

    #[test]
    fn test_ext_from_content_type_all() {
        assert_eq!(ext_from_content_type("image/gif"), ".gif");
        assert_eq!(ext_from_content_type("image/webp"), ".webp");
        assert_eq!(ext_from_content_type("audio/mp3"), ".mp3");
        assert_eq!(ext_from_content_type("audio/mp4"), ".m4a");
        assert_eq!(ext_from_content_type("video/3gpp"), ".3gp");
    }

    #[test]
    fn test_clean_number_plain() {
        assert_eq!(TwilioChannel::clean_number("12345"), "12345");
    }

    #[test]
    fn test_base64_encode_longer() {
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_encode(b"M"), "TQ==");
    }

    #[test]
    fn test_verify_signature_matching() {
        // Compute what the signature should be for known inputs
        let auth_token = "mytoken";
        let url = "https://example.com/webhook";
        let params = vec![
            ("Body".to_string(), "Hello".to_string()),
            ("From".to_string(), "+1234".to_string()),
        ];

        let sig1 = {
            let mut sorted = params.clone();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            let mut data = url.to_string();
            for (k, v) in &sorted {
                data.push_str(k);
                data.push_str(v);
            }
            base64_encode(&hmac_sha1(auth_token.as_bytes(), data.as_bytes()))
        };

        assert!(verify_twilio_signature(auth_token, url, &params, &sig1));
        assert!(!verify_twilio_signature(auth_token, url, &params, "wrong"));
    }
}
