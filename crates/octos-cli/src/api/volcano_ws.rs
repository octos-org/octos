//! Volcano Engine **v1** TTS WebSocket binary protocol
//! (`wss://openspeech.bytedance.com/api/v1/tts/ws_binary`, `operation:"submit"`).
//!
//! Same auth/body/cluster/voice as the existing v1 HTTP `synthesize_volcano`
//! (`Authorization: Bearer;{token}` + `app{appid,token,cluster}` body), but the
//! request rides a binary frame over WebSocket and the server streams audio back
//! as a sequence of "audio-only" frames until a negative sequence number marks
//! the last one. Keeps BV001 (a free v1 voice) — the V3 endpoint rejects it.
//!
//! Frame layout (big-endian), per the v1 ws_binary spec:
//!
//! ```text
//! byte 0:  (protocol_version<<4) | header_size        // 0x11 = ver1, header 4B
//! byte 1:  (message_type<<4)     | type_specific_flags
//! byte 2:  (serialization<<4)    | compression         // JSON=0x1, gzip=0x1/none=0x0
//! byte 3:  reserved (0x00)
//! [audio/error frames may carry a 4B sequence after the header]
//! next 4:  payload size (u32 BE)
//! rest:    payload (request: JSON; audio response: raw audio bytes)
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// Default v1 ws_binary endpoint; overridable via `VOLC_TTS_WS_ENDPOINT`.
const WS_ENDPOINT: &str = "wss://openspeech.bytedance.com/api/v1/tts/ws_binary";

/// Build an explicit rustls TLS connector (ring provider + native roots).
/// Mirrors the octos-bus WS channels: the process installs no global default
/// `CryptoProvider`, so an explicit config avoids rustls' auto-detection panic.
fn make_tls_connector() -> Option<tokio_tungstenite::Connector> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        roots.add(cert).ok();
    }
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .inspect_err(|e| tracing::warn!(error = %e, "voice_turn: volcano ws TLS config failed"))
        .ok()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Some(tokio_tungstenite::Connector::Rustls(Arc::new(config)))
}

/// True only for a `wss://` URL whose host is in the shared Volcano allowlist
/// ([`crate::api::voice_turn::VOLCANO_ALLOWED_HOSTS`]). This is the same SSRF /
/// token-exfiltration boundary the HTTP path enforces before sending the
/// token: the `Authorization: Bearer;{token}` header and the body token ride
/// this URL, so an off-allowlist (or plaintext `ws://`) override must never
/// see credentials.
fn is_allowed_ws_endpoint(endpoint: &str) -> bool {
    match reqwest::Url::parse(endpoint) {
        Ok(u) => {
            u.scheme() == "wss"
                && u.host_str()
                    .is_some_and(|h| crate::api::voice_turn::VOLCANO_ALLOWED_HOSTS.contains(&h))
        }
        Err(_) => false,
    }
}

/// Resolve the ws endpoint. An unset/empty `VOLC_TTS_WS_ENDPOINT` yields the
/// default; a set override must pass [`is_allowed_ws_endpoint`] or it is
/// REFUSED (`None` — the token is never attached), and the caller degrades to
/// the allowlist-validated HTTP path in [`crate::api::voice_turn`].
fn ws_endpoint() -> Option<String> {
    match std::env::var("VOLC_TTS_WS_ENDPOINT") {
        Ok(ep) if !ep.is_empty() => {
            if is_allowed_ws_endpoint(&ep) {
                Some(ep)
            } else {
                tracing::warn!(
                    endpoint = %ep,
                    "voice_turn: refusing volcano ws TTS — endpoint not in the wss Volcano allowlist; token NOT sent"
                );
                None
            }
        }
        _ => Some(WS_ENDPOINT.to_string()),
    }
}

/// File extension for the requested audio encoding.
fn audio_ext(encoding: &str) -> &'static str {
    match encoding {
        "wav" => "wav",
        "pcm" => "pcm",
        "ogg_opus" => "ogg",
        _ => "mp3",
    }
}

/// Streaming core: open the v1 ws_binary connection, send the `submit`
/// request, and invoke `on_chunk` for each audio chunk as it arrives. Returns
/// `Some(())` on a clean end (final negative-sequence frame), `None` on any
/// transport/protocol failure. Shared by the collect→file path ([`synthesize_ws`])
/// and the ⑤ push-to-client path.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn synthesize_ws_stream(
    appid: &str,
    token: &str,
    cluster: &str,
    voice: &str,
    encoding: &str,
    text: &str,
    mut on_chunk: impl FnMut(&[u8], bool),
) -> Option<()> {
    let reqid = uuid::Uuid::now_v7().to_string();
    let payload = build_submit_payload(appid, token, cluster, voice, encoding, text, &reqid);
    let frame = encode_request_frame(&payload, false);

    let mut request = ws_endpoint()?
        .into_client_request()
        .inspect_err(|e| tracing::warn!(error = %e, "voice_turn: volcano ws bad endpoint"))
        .ok()?;
    // Same quirky scheme as the HTTP path: literal "Bearer;" + token. The
    // resource is resolved server-side from the body `cluster` (volcano_tts),
    // exactly like the HTTP v1 path — no resource header per the v1 ws spec.
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer;{token}").parse().ok()?);

    let connector = make_tls_connector()?;
    let (mut ws, _resp) =
        tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(connector))
            .await
            .inspect_err(|e| tracing::warn!(error = %e, "voice_turn: volcano ws connect failed"))
            .ok()?;

    ws.send(Message::Binary(frame.into()))
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "voice_turn: volcano ws send failed"))
        .ok()?;

    while let Some(msg) = ws.next().await {
        let msg = msg
            .inspect_err(|e| tracing::warn!(error = %e, "voice_turn: volcano ws recv failed"))
            .ok()?;
        match msg {
            Message::Binary(data) => match parse_server_frame(&data) {
                Ok(ServerFrame::Audio { data, is_last }) => {
                    on_chunk(&data, is_last);
                    if is_last {
                        break;
                    }
                }
                Ok(ServerFrame::Error { code, message }) => {
                    tracing::warn!(code, %message, "voice_turn: volcano ws error frame");
                    return None;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "voice_turn: volcano ws frame parse failed");
                    return None;
                }
            },
            Message::Close(_) => break,
            _ => {} // ping/pong/text: ignore
        }
    }
    Some(())
}

/// Collect→file wrapper over [`synthesize_ws_stream`]: buffers every chunk and
/// writes one file under `out_dir` (drop-in for the HTTP `query` path). The
/// streaming push-to-client path (⑤) uses `synthesize_ws_stream` directly.
#[allow(dead_code)] // wired into synthesize_reply later.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn synthesize_ws(
    appid: &str,
    token: &str,
    cluster: &str,
    voice: &str,
    encoding: &str,
    text: &str,
    out_dir: &Path,
) -> Option<PathBuf> {
    let mut audio: Vec<u8> = Vec::new();
    synthesize_ws_stream(appid, token, cluster, voice, encoding, text, |c, _last| {
        audio.extend_from_slice(c)
    })
    .await?;

    if audio.is_empty() {
        return None;
    }
    let out_path = out_dir.join(format!(
        "reply-{}.{}",
        uuid::Uuid::now_v7(),
        audio_ext(encoding)
    ));
    tokio::fs::write(&out_path, &audio)
        .await
        .inspect_err(|e| tracing::warn!(error = %e, "voice_turn: volcano ws write failed"))
        .ok()?;
    Some(out_path)
}

/// Build a "full client request" frame: 4-byte header + 4-byte big-endian
/// payload length + payload. `gzip` only sets the compression nibble — the
/// caller gzips `payload` itself when true.
fn encode_request_frame(payload: &[u8], gzip: bool) -> Vec<u8> {
    // header byte 2: serialization JSON (0b0001) in the high nibble, compression
    // (gzip 0b0001 / none 0b0000) in the low nibble.
    let serialization_compression = 0x10 | if gzip { 0x01 } else { 0x00 };
    let mut frame = Vec::with_capacity(8 + payload.len());
    frame.extend_from_slice(&[
        0x11, // version 1, header size 1 (×4 = 4 bytes)
        0x10, // message type 0b0001 (full client request), flags 0b0000
        serialization_compression,
        0x00, // reserved
    ]);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Build the JSON request body for a v1 ws `submit` (streaming) synthesis.
/// Same shape as the HTTP `query` body, with `operation:"submit"`. `reqid` is a
/// caller-supplied unique id (passed in so the body is deterministic to test).
#[allow(dead_code)] // wired into the synth loop later in this change.
#[allow(clippy::too_many_arguments)]
fn build_submit_payload(
    appid: &str,
    token: &str,
    cluster: &str,
    voice: &str,
    encoding: &str,
    text: &str,
    reqid: &str,
) -> Vec<u8> {
    let body = serde_json::json!({
        "app": { "appid": appid, "token": token, "cluster": cluster },
        "user": { "uid": "octos-voice" },
        "audio": { "voice_type": voice, "encoding": encoding, "speed_ratio": 1.0 },
        "request": { "reqid": reqid, "text": text, "operation": "submit" },
    });
    serde_json::to_vec(&body).expect("serialize volcano ws submit body")
}

/// A decoded server frame from the v1 ws_binary stream.
#[allow(dead_code)] // wired into the synth loop later in this change.
#[derive(Debug, PartialEq)]
enum ServerFrame {
    /// An audio-only response chunk. `is_last` is set when the frame's sequence
    /// number is negative (the server's final chunk).
    Audio { data: Vec<u8>, is_last: bool },
    /// A server error frame (message type 0b1111): code + text message.
    Error { code: u32, message: String },
}

/// Parse one server binary frame. Returns `Err` on a malformed/truncated frame
/// or an unexpected message type. Bounds-checked so a short frame can't panic.
#[allow(dead_code)] // wired into the synth loop later in this change.
fn parse_server_frame(bytes: &[u8]) -> Result<ServerFrame, String> {
    // Read a big-endian u32 at `off`, advancing it; error if out of range.
    fn take_u32(bytes: &[u8], off: &mut usize) -> Result<u32, String> {
        let end = *off + 4;
        let slice = bytes
            .get(*off..end)
            .ok_or_else(|| "truncated frame: expected 4 more bytes".to_string())?;
        *off = end;
        Ok(u32::from_be_bytes(slice.try_into().expect("4-byte slice")))
    }

    if bytes.len() < 4 {
        return Err("frame shorter than header".to_string());
    }
    let header_len = (bytes[0] & 0x0f) as usize * 4;
    let message_type = bytes[1] >> 4;
    let flags = bytes[1] & 0x0f;
    let mut off = header_len;

    match message_type {
        // Audio-only server response (0b1011): optional sequence, size, raw audio.
        0b1011 => {
            let is_last = if flags != 0 {
                let seq = take_u32(bytes, &mut off)? as i32;
                seq < 0
            } else {
                false
            };
            let size = take_u32(bytes, &mut off)? as usize;
            let data = bytes
                .get(off..off + size)
                .ok_or_else(|| "truncated frame: audio payload short".to_string())?
                .to_vec();
            Ok(ServerFrame::Audio { data, is_last })
        }
        // Error message from server (0b1111): code + sized text message.
        0b1111 => {
            let code = take_u32(bytes, &mut off)?;
            let size = take_u32(bytes, &mut off)? as usize;
            let msg = bytes
                .get(off..off + size)
                .ok_or_else(|| "truncated frame: error message short".to_string())?;
            Ok(ServerFrame::Error {
                code,
                message: String::from_utf8_lossy(msg).into_owned(),
            })
        }
        other => Err(format!("unexpected server message type 0b{other:04b}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_accept_default_ws_endpoint_when_checking_allowlist() {
        assert!(is_allowed_ws_endpoint(WS_ENDPOINT));
    }

    #[test]
    fn should_reject_off_host_ws_endpoint_to_prevent_token_exfiltration() {
        // An env-overridden endpoint pointing off the Volcano allowlist must
        // never see the Authorization header / body token.
        assert!(!is_allowed_ws_endpoint(
            "wss://evil.example.com/api/v1/tts/ws_binary"
        ));
        // Allowlisted host as a suffix of an attacker domain must not pass.
        assert!(!is_allowed_ws_endpoint(
            "wss://openspeech.bytedance.com.evil.example/api/v1/tts/ws_binary"
        ));
        // Userinfo trick: the allowlisted name in the userinfo position must
        // not fool the check — the parsed host is evil.example.
        assert!(!is_allowed_ws_endpoint(
            "wss://openspeech.bytedance.com@evil.example/api/v1/tts/ws_binary"
        ));
    }

    #[test]
    fn should_reject_non_wss_scheme_ws_endpoint() {
        // Plaintext ws:// would leak the token on the wire; https:// is not a
        // WebSocket endpoint at all.
        assert!(!is_allowed_ws_endpoint(
            "ws://openspeech.bytedance.com/api/v1/tts/ws_binary"
        ));
        assert!(!is_allowed_ws_endpoint(
            "https://openspeech.bytedance.com/api/v1/tts/ws_binary"
        ));
    }

    #[test]
    fn should_reject_unparseable_ws_endpoint() {
        assert!(!is_allowed_ws_endpoint("not a url"));
        assert!(!is_allowed_ws_endpoint(""));
    }

    #[test]
    fn request_frame_has_4byte_header_then_be_length_then_payload() {
        let payload = b"{}";
        let frame = encode_request_frame(payload, false);
        // header: version1/headerSize1, full-client-request/no-flags,
        // JSON/no-compression, reserved.
        assert_eq!(&frame[0..4], &[0x11, 0x10, 0x10, 0x00]);
        // payload length as big-endian u32 == 2
        assert_eq!(&frame[4..8], &[0x00, 0x00, 0x00, 0x02]);
        assert_eq!(&frame[8..], payload);
    }

    #[test]
    fn request_frame_sets_gzip_compression_nibble() {
        let frame = encode_request_frame(b"x", true);
        // byte 2: serialization JSON (0x1) high nibble | gzip (0x1) low nibble.
        assert_eq!(frame[2], 0x11);
    }

    #[test]
    fn submit_payload_has_operation_submit_and_core_fields() {
        let p = build_submit_payload(
            "APP",
            "TOK",
            "volcano_tts",
            "BV001_streaming",
            "mp3",
            "你好",
            "rid-1",
        );
        let v: serde_json::Value = serde_json::from_slice(&p).unwrap();
        assert_eq!(v["app"]["appid"], "APP");
        assert_eq!(v["app"]["token"], "TOK");
        assert_eq!(v["app"]["cluster"], "volcano_tts");
        assert_eq!(v["audio"]["voice_type"], "BV001_streaming");
        assert_eq!(v["audio"]["encoding"], "mp3");
        assert_eq!(v["request"]["operation"], "submit");
        assert_eq!(v["request"]["text"], "你好");
        assert_eq!(v["request"]["reqid"], "rid-1");
    }

    #[test]
    fn parses_audio_frame_with_positive_sequence_as_not_last() {
        // header: msg_type 0b1011 (audio), flags 0b0001 (sequence > 0)
        let mut f = vec![0x11, 0xB1, 0x00, 0x00];
        f.extend_from_slice(&1i32.to_be_bytes()); // sequence = 1
        f.extend_from_slice(&3u32.to_be_bytes()); // payload size = 3
        f.extend_from_slice(&[0xAA, 0xBB, 0xCC]); // audio bytes
        assert_eq!(
            parse_server_frame(&f).unwrap(),
            ServerFrame::Audio {
                data: vec![0xAA, 0xBB, 0xCC],
                is_last: false
            }
        );
    }

    #[test]
    fn parses_audio_frame_with_negative_sequence_as_last() {
        // flags 0b0011 → sequence < 0 → final chunk
        let mut f = vec![0x11, 0xB3, 0x00, 0x00];
        f.extend_from_slice(&(-1i32).to_be_bytes()); // sequence = -1
        f.extend_from_slice(&2u32.to_be_bytes());
        f.extend_from_slice(&[0x01, 0x02]);
        assert_eq!(
            parse_server_frame(&f).unwrap(),
            ServerFrame::Audio {
                data: vec![0x01, 0x02],
                is_last: true
            }
        );
    }

    #[test]
    fn parses_error_frame_with_code_and_message() {
        // header: msg_type 0b1111 (error), no flags
        let mut f = vec![0x11, 0xF0, 0x00, 0x00];
        f.extend_from_slice(&3050u32.to_be_bytes()); // error code
        f.extend_from_slice(&5u32.to_be_bytes()); // message size
        f.extend_from_slice(b"hello");
        assert_eq!(
            parse_server_frame(&f).unwrap(),
            ServerFrame::Error {
                code: 3050,
                message: "hello".to_string()
            }
        );
    }

    #[test]
    fn rejects_truncated_frame_without_panicking() {
        // audio frame header claims a sequence + size but the buffer ends early.
        let f = vec![0x11, 0xB1, 0x00, 0x00, 0x00, 0x00];
        assert!(parse_server_frame(&f).is_err());
    }

    // Live check against the real v1 ws_binary endpoint. Ignored by default;
    // run on a box with creds:
    //   VOLC_TTS_APPID=… VOLC_TTS_TOKEN=… \
    //   cargo test -p octos-cli --lib --features api volcano_ws -- --ignored
    #[tokio::test]
    #[ignore = "hits live Volcano ws_binary; needs VOLC_TTS_APPID + VOLC_TTS_TOKEN"]
    async fn live_synthesize_bv001_writes_audio() {
        let appid = std::env::var("VOLC_TTS_APPID").expect("VOLC_TTS_APPID");
        let token = std::env::var("VOLC_TTS_TOKEN").expect("VOLC_TTS_TOKEN");
        let dir = std::env::temp_dir();
        let path = synthesize_ws(
            &appid,
            &token,
            "volcano_tts",
            "BV001_streaming",
            "mp3",
            "你好，这是一次流式合成测试。",
            &dir,
        )
        .await
        .expect("ws synth returned None");
        let len = std::fs::metadata(&path).unwrap().len();
        assert!(len > 1000, "audio too small: {len} bytes");
        let _ = std::fs::remove_file(&path);
    }
}
