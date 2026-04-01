//! Message preprocessing: media transcription, cron routing, and reply helpers.
//!
//! Extracted from the gateway main loop to keep `mod.rs` focused on
//! orchestration and dispatch.

use std::path::Path;

use eyre::WrapErr;
use octos_bus::ChannelManager;
use octos_core::{InboundMessage, OutboundMessage};
use tracing::warn;

/// Result of media preprocessing on an inbound message.
pub struct MediaResult {
    /// Image file paths extracted from `inbound.media`.
    pub image_media: Vec<String>,
    /// Whether any audio attachment was detected (for auto-TTS downstream).
    #[allow(dead_code)]
    pub is_voice_message: bool,
}

/// Transcribe audio, separate images, and tag voice metadata on an inbound message.
///
/// Mutates `inbound.content` (prepends transcript) and `inbound.metadata`
/// (inserts `voice_transcript` and `voice_message` keys).
pub async fn process_media(
    inbound: &mut InboundMessage,
    asr_binary: Option<&Path>,
    asr_language: Option<&str>,
    channel_mgr: &ChannelManager,
) -> MediaResult {
    let mut image_media = Vec::new();
    let mut is_voice_message = false;

    if let Some(asr_bin) = asr_binary {
        for path in &inbound.media {
            if octos_bus::media::is_audio(path) {
                is_voice_message = true;
                // Show "listening" indicator while transcribing voice
                if let Some(ch) = channel_mgr.get_channel(&inbound.channel) {
                    let _ = ch.send_listening(&inbound.chat_id).await;
                }
                let mut input = serde_json::json!({"audio_path": path});
                if let Some(lang) = asr_language {
                    input["language"] = serde_json::Value::String(lang.to_string());
                }
                match transcribe_via_skill(asr_bin, &input.to_string()).await {
                    Ok(text) => {
                        if let Some(obj) = inbound.metadata.as_object_mut() {
                            obj.insert(
                                "voice_transcript".into(),
                                serde_json::Value::String(text.clone()),
                            );
                        }
                        let prefix = format!("[Voice transcription: {text}]\n\n");
                        inbound.content = format!("{prefix}{}", inbound.content);
                    }
                    Err(e) => warn!("transcription failed: {e}"),
                }
                // Always append audio file path so agent can use it
                // for voice_clone / voice_save_profile if conversation
                // context calls for it.
                inbound.content.push_str(&format!("\n[Audio file: {path}]"));
            } else if octos_bus::media::is_image(path) {
                image_media.push(path.clone());
            }
        }
    } else {
        // Check for audio even without transcriber (for voice_message flag)
        for path in &inbound.media {
            if octos_bus::media::is_audio(path) {
                is_voice_message = true;
            } else if octos_bus::media::is_image(path) {
                image_media.push(path.clone());
            }
        }
    }

    // Tag voice messages in metadata for auto-TTS downstream
    if is_voice_message {
        if let Some(obj) = inbound.metadata.as_object_mut() {
            obj.insert("voice_message".into(), serde_json::Value::Bool(true));
        }
    }

    MediaResult {
        image_media,
        is_voice_message,
    }
}

/// Resolve the reply channel and chat_id for an inbound message.
///
/// Cron/heartbeat messages arrive on the `"system"` channel and carry
/// `deliver_to_channel` / `deliver_to_chat_id` in metadata. For all other
/// channels the reply target is the same as the inbound source.
pub fn resolve_reply_target(
    inbound: &InboundMessage,
    default_cron_channel: &str,
    default_cron_chat_id: &str,
) -> (String, String) {
    if inbound.channel == "system" {
        let ch = inbound
            .metadata
            .get("deliver_to_channel")
            .and_then(|v| v.as_str())
            .and_then(|s| if s.is_empty() { None } else { Some(s) })
            .unwrap_or(default_cron_channel)
            .to_string();
        let cid = inbound
            .metadata
            .get("deliver_to_chat_id")
            .and_then(|v| v.as_str())
            .and_then(|s| if s.is_empty() { None } else { Some(s) })
            .unwrap_or_else(|| {
                if !default_cron_chat_id.is_empty() {
                    default_cron_chat_id
                } else {
                    &inbound.chat_id
                }
            })
            .to_string();
        (ch, cid)
    } else {
        (inbound.channel.clone(), inbound.chat_id.clone())
    }
}

/// Build a simple text reply to send back on the same channel/chat.
pub fn make_reply(channel: &str, chat_id: &str, content: impl Into<String>) -> OutboundMessage {
    OutboundMessage {
        channel: channel.to_string(),
        chat_id: chat_id.to_string(),
        content: content.into(),
        reply_to: None,
        media: vec![],
        metadata: serde_json::json!({}),
    }
}

/// Merge queued inbound messages by session key.
/// Messages from the same session are concatenated with `\n\n`.
/// Used by Collect queue mode (reserved for future concurrent collect support).
#[allow(dead_code)]
pub fn merge_queued_by_session(messages: Vec<InboundMessage>) -> Vec<InboundMessage> {
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<String, Vec<InboundMessage>> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for msg in messages {
        let key = msg.session_key().to_string();
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(msg);
    }
    order
        .into_iter()
        .filter_map(|key| {
            let mut msgs = groups.remove(&key)?;
            if msgs.len() == 1 {
                return msgs.pop();
            }
            let mut base = msgs.remove(0);
            for m in &msgs {
                base.content.push_str("\n\n");
                base.content.push_str(&m.content);
            }
            Some(base)
        })
        .collect()
}

/// Transcribe audio by spawning the voice platform skill binary.
async fn transcribe_via_skill(voice_binary: &Path, input_json: &str) -> eyre::Result<String> {
    use tokio::io::AsyncWriteExt;

    let mut child = tokio::process::Command::new(voice_binary)
        .arg("voice_transcribe")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .wrap_err("failed to spawn voice skill binary")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input_json.as_bytes()).await?;
        drop(stdin);
    }

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| eyre::eyre!("voice transcription timed out"))??;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let result: serde_json::Value =
        serde_json::from_str(&stdout).wrap_err("invalid voice skill output")?;

    if result.get("success").and_then(|v| v.as_bool()) == Some(true) {
        Ok(result["output"].as_str().unwrap_or("").to_string())
    } else {
        let msg = result["output"].as_str().unwrap_or("unknown error");
        eyre::bail!("voice skill failed: {msg}")
    }
}
