//! Message preprocessing: media transcription, cron routing, and reply helpers.
//!
//! Extracted from the gateway main loop to keep `mod.rs` focused on
//! orchestration and dispatch.

use std::path::{Path, PathBuf};

use eyre::WrapErr;
use octos_bus::ChannelManager;
use octos_core::{InboundMessage, OutboundMessage};
use tracing::warn;

const MAX_SCANNED_PDF_FALLBACK_BYTES: u64 = 25 * 1024 * 1024;
const MAX_SCANNED_PDF_FALLBACK_PAGES: u32 = 3;
const SCANNED_PDF_FALLBACK_DPI: u32 = 120;
const MIN_MEANINGFUL_PDF_ALNUM_CHARS: usize = 24;

/// Result of media preprocessing on an inbound message.
pub struct MediaResult {
    /// Image file paths extracted from `inbound.media`.
    pub image_media: Vec<String>,
    /// Non-image attachments copied into the workspace for tool access.
    pub attachment_media: Vec<String>,
    /// Transient attachment summary for the current turn only.
    pub attachment_prompt: Option<String>,
    /// Whether any audio attachment was detected (for auto-TTS downstream).
    #[allow(dead_code)]
    pub is_voice_message: bool,
    /// All audio inputs returned a successful empty transcript, with no typed
    /// prompt or non-audio attachment to process. This is a normal ASR reject.
    pub rejected_audio_only: bool,
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
    let mut attachment_media = Vec::new();
    let mut is_voice_message = false;
    let mut audio_filenames = Vec::new();
    let mut attachment_filenames = Vec::new();
    let mut attachment_notes = Vec::new();
    let mut saw_audio = false;
    let mut all_audio_transcripts_rejected = asr_binary.is_some();

    if let Some(asr_bin) = asr_binary {
        for path in &inbound.media {
            let resolved_path = resolve_media_reference(path);
            if octos_bus::media::is_audio(path) {
                saw_audio = true;
                is_voice_message = true;
                attachment_media.push(resolved_path.clone());
                audio_filenames.push(attachment_display_name(path));
                // Show "listening" indicator while transcribing voice
                if let Some(ch) = channel_mgr.get_channel(&inbound.channel) {
                    let _ = ch.send_listening(&inbound.chat_id).await;
                }
                let mut input = serde_json::json!({"audio_path": resolved_path});
                if let Some(lang) = asr_language {
                    input["language"] = serde_json::Value::String(lang.to_string());
                }
                match transcribe_via_skill(asr_bin, &input.to_string()).await {
                    Ok(text) if text.trim().is_empty() => {
                        tracing::info!(audio = %resolved_path, "ASR rejected audio without speech");
                    }
                    Ok(text) => {
                        all_audio_transcripts_rejected = false;
                        if let Some(obj) = inbound.metadata.as_object_mut() {
                            obj.insert(
                                "voice_transcript".into(),
                                serde_json::Value::String(text.clone()),
                            );
                        }
                        inbound.content = merge_transcript_into_content(&inbound.content, &text);
                    }
                    Err(e) => {
                        all_audio_transcripts_rejected = false;
                        warn!("transcription failed: {e}");
                    }
                }
            } else {
                route_non_audio_attachment(
                    &resolved_path,
                    path,
                    &mut image_media,
                    &mut attachment_media,
                    &mut attachment_filenames,
                    &mut attachment_notes,
                )
                .await;
            }
        }
    } else {
        // Check for audio even without transcriber (for voice_message flag)
        for path in &inbound.media {
            let resolved_path = resolve_media_reference(path);
            if octos_bus::media::is_audio(path) {
                is_voice_message = true;
                attachment_media.push(resolved_path.clone());
                audio_filenames.push(attachment_display_name(path));
            } else {
                route_non_audio_attachment(
                    &resolved_path,
                    path,
                    &mut image_media,
                    &mut attachment_media,
                    &mut attachment_filenames,
                    &mut attachment_notes,
                )
                .await;
            }
        }
    }

    let attachment_prompt = build_attachment_summary("Attached audio files", &audio_filenames)
        .into_iter()
        .chain(build_attachment_summary(
            "Attached files",
            &attachment_filenames,
        ))
        .chain(build_attachment_summary(
            "PDF processing",
            &attachment_notes,
        ))
        .collect::<Vec<_>>()
        .join("\n\n");

    // Tag voice messages in metadata for auto-TTS downstream
    if is_voice_message {
        if let Some(obj) = inbound.metadata.as_object_mut() {
            obj.insert("voice_message".into(), serde_json::Value::Bool(true));
        }
    }

    let rejected_audio_only = should_skip_rejected_audio_only_turn(
        saw_audio && all_audio_transcripts_rejected,
        &inbound.content,
        !image_media.is_empty() || !attachment_filenames.is_empty(),
    );

    MediaResult {
        image_media,
        attachment_media,
        attachment_prompt: if attachment_prompt.is_empty() {
            None
        } else {
            Some(attachment_prompt)
        },
        is_voice_message,
        rejected_audio_only,
    }
}

/// A successful empty ASR result is only terminal when this message contains
/// no other user input. Mixed text/audio and attachment turns must proceed.
fn should_skip_rejected_audio_only_turn(
    all_audio_rejected: bool,
    content: &str,
    has_non_audio_media: bool,
) -> bool {
    all_audio_rejected && content.trim().is_empty() && !has_non_audio_media
}

async fn route_non_audio_attachment(
    resolved_path: &str,
    display_source: &str,
    image_media: &mut Vec<String>,
    attachment_media: &mut Vec<String>,
    attachment_filenames: &mut Vec<String>,
    attachment_notes: &mut Vec<String>,
) {
    if octos_bus::media::is_image(display_source) {
        image_media.push(resolved_path.to_string());
    } else {
        attachment_media.push(resolved_path.to_string());
        let display_name = attachment_display_name(display_source);
        attachment_filenames.push(display_name.clone());
        maybe_add_scanned_pdf_fallback(resolved_path, &display_name, image_media, attachment_notes)
            .await;
    }
}

async fn maybe_add_scanned_pdf_fallback(
    resolved_path: &str,
    display_name: &str,
    image_media: &mut Vec<String>,
    attachment_notes: &mut Vec<String>,
) {
    let path = Path::new(resolved_path);
    if !path.exists() || !has_pdf_magic(path) {
        return;
    }

    match path.metadata().map(|meta| meta.len()) {
        Ok(size) if size > MAX_SCANNED_PDF_FALLBACK_BYTES => {
            attachment_notes.push(pdf_fallback_size_limit_note(display_name, size));
            return;
        }
        Ok(_) => {}
        Err(error) => {
            attachment_notes.push(format!(
                "{display_name}: PDF page-image fallback skipped: failed to read file metadata: {error}"
            ));
            return;
        }
    }

    let text_status = classify_pdf_text(path).await;
    if text_status == PdfTextStatus::Meaningful {
        return;
    }

    let render_result = render_pdf_pages_as_images(path).await;
    record_pdf_fallback_result(
        display_name,
        text_status,
        render_result,
        image_media,
        attachment_notes,
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PdfTextStatus {
    Meaningful,
    EmptyOrSparse,
    ExtractionFailed(String),
}

async fn classify_pdf_text(path: &Path) -> PdfTextStatus {
    match octos_agent::tools::read_no_follow(path).await {
        Ok(text) if has_meaningful_pdf_text(&text) => PdfTextStatus::Meaningful,
        Ok(_) => PdfTextStatus::EmptyOrSparse,
        Err(error) => PdfTextStatus::ExtractionFailed(error.to_string()),
    }
}

fn has_meaningful_pdf_text(text: &str) -> bool {
    text.chars().filter(|ch| ch.is_alphanumeric()).count() >= MIN_MEANINGFUL_PDF_ALNUM_CHARS
}

fn has_pdf_magic(path: &Path) -> bool {
    use std::io::Read;

    if path.symlink_metadata().is_ok_and(|meta| meta.is_symlink()) {
        return false;
    }

    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 5];
    matches!(file.read(&mut magic), Ok(n) if n >= 5 && &magic == b"%PDF-")
}

fn record_pdf_fallback_result(
    display_name: &str,
    text_status: PdfTextStatus,
    render_result: Result<Vec<String>, String>,
    image_media: &mut Vec<String>,
    attachment_notes: &mut Vec<String>,
) {
    let reason = match text_status {
        PdfTextStatus::Meaningful => return,
        PdfTextStatus::EmptyOrSparse => {
            "PDF text extraction produced no meaningful text".to_string()
        }
        PdfTextStatus::ExtractionFailed(error) => {
            format!("PDF text extraction failed: {error}")
        }
    };

    match render_result {
        Ok(pages) if !pages.is_empty() => {
            let count = pages.len();
            let page_labels = (1..=count)
                .map(|page| format!("{display_name} page {page}"))
                .collect::<Vec<_>>()
                .join(", ");
            image_media.extend(pages);
            attachment_notes.push(format!(
                "{display_name}: {reason}; rendered first {count} page(s) as images for vision-capable models ({page_labels}; bounded to {MAX_SCANNED_PDF_FALLBACK_PAGES} page(s))"
            ));
        }
        Ok(_) => attachment_notes.push(format!(
            "{display_name}: {reason}; page-image fallback failed: renderer produced no pages"
        )),
        Err(error) => attachment_notes.push(format!(
            "{display_name}: {reason}; page-image fallback failed: {error}"
        )),
    }
}

fn pdf_fallback_size_limit_note(display_name: &str, size: u64) -> String {
    format!(
        "{display_name}: PDF page-image fallback skipped: file size {size} bytes exceeds {MAX_SCANNED_PDF_FALLBACK_BYTES} byte limit"
    )
}

async fn render_pdf_pages_as_images(pdf_path: &Path) -> Result<Vec<String>, String> {
    let pdf_path = pdf_path.to_owned();
    tokio::task::spawn_blocking(move || render_pdf_pages_as_images_blocking(&pdf_path))
        .await
        .unwrap_or_else(|error| Err(format!("renderer task failed: {error}")))
}

fn render_pdf_pages_as_images_blocking(pdf_path: &Path) -> Result<Vec<String>, String> {
    let output_dir = prepare_pdf_output_dir(&octos_bus::file_handle::temp_upload_root())?;

    let prefix_name = format!("{}-{}-page", uuid::Uuid::now_v7(), safe_file_stem(pdf_path));
    let output_prefix = output_dir.join(&prefix_name);
    let mut cmd = std::process::Command::new("pdftoppm");
    cmd.arg("-png")
        .arg("-r")
        .arg(SCANNED_PDF_FALLBACK_DPI.to_string())
        .arg("-f")
        .arg("1")
        .arg("-l")
        .arg(MAX_SCANNED_PDF_FALLBACK_PAGES.to_string())
        .arg(pdf_path)
        .arg(&output_prefix);
    // R-17: a bare `.output()` waits forever; a stuck renderer parked the
    // preprocessing thread (and with it the inbound message) indefinitely.
    let (status, stderr) =
        crate::process_util::run_with_deadline(&mut cmd, crate::process_util::RENDERER_DEADLINE)
            .map_err(|error| match &error {
                crate::process_util::BoundedRunError::Io(e)
                    if e.kind() == std::io::ErrorKind::NotFound =>
                {
                    "pdftoppm not found (install poppler)".to_string()
                }
                _ => format!("failed to run pdftoppm: {error}"),
            })?;

    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        return Err(format!("pdftoppm failed: {}", stderr.trim()));
    }

    collect_rendered_pdf_pages(&output_dir, &prefix_name)
}

fn prepare_pdf_output_dir(upload_root: &Path) -> Result<PathBuf, String> {
    let output_dir = upload_root.join("pdf-page-images");
    // PDF rendering can bypass the upload handlers. Validate the root first so
    // a foreign-owned root is rejected before creating any private page data.
    octos_bus::media::ensure_private_dir(upload_root)
        .and_then(|()| octos_bus::media::ensure_private_dir(&output_dir))
        .map_err(|error| format!("failed to create rendered-page directory: {error}"))?;

    Ok(output_dir)
}

fn collect_rendered_pdf_pages(output_dir: &Path, prefix_name: &str) -> Result<Vec<String>, String> {
    let mut pages: Vec<PathBuf> = std::fs::read_dir(output_dir)
        .map_err(|error| format!("failed to list rendered pages: {error}"))?
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_str()?;
            if name.starts_with(prefix_name) && name.ends_with(".png") {
                Some(path)
            } else {
                None
            }
        })
        .collect();
    pages.sort_by(|left, right| left.file_name().cmp(&right.file_name()));

    Ok(pages
        .into_iter()
        .map(|path| {
            path.canonicalize()
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned()
        })
        .collect())
}

fn safe_file_stem(path: &Path) -> String {
    let raw = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("pdf");
    let mut safe = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .take(40)
        .collect::<String>();
    if safe.trim_matches('-').is_empty() {
        safe = "pdf".to_string();
    }
    safe
}

fn resolve_media_reference(path: &str) -> String {
    octos_bus::file_handle::resolve_upload_reference(path)
        .map(|resolved| resolved.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

fn merge_transcript_into_content(existing: &str, transcript: &str) -> String {
    if existing.trim().is_empty() {
        transcript.to_string()
    } else {
        format!("Transcribed audio:\n{transcript}\n\n{existing}")
    }
}

fn attachment_display_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
        .to_string()
}

fn build_attachment_summary(heading: &str, filenames: &[String]) -> Option<String> {
    if filenames.is_empty() {
        return None;
    }

    Some(format!(
        "[{heading}]\n{}",
        filenames
            .iter()
            .map(|name| format!("- {name}"))
            .collect::<Vec<_>>()
            .join("\n")
    ))
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
            .filter(|s| !s.is_empty())
            .unwrap_or(default_cron_channel)
            .to_string();
        let cid = inbound
            .metadata
            .get("deliver_to_chat_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
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
