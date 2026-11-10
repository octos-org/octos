//! Media a tool handed the model.
//!
//! A tool that wants the model to look at something (`view_image`,
//! `view_video`) returns paths as `ToolResult::model_media`; the agent loop
//! puts them on the tool row's `media`. Nothing else is added to the
//! transcript. Each provider then renders that media in its own correct
//! shape — Anthropic inside the `tool_result` block, Gemini as a multimodal
//! function response, the OpenAI-compatible and Responses protocols as a
//! user turn built at wire time after the batch's tool outputs. Recent images
//! remain visible through intervening text tools. Eviction is explicit; a note
//! saying an image used to be shown is not a replacement for its pixels.
//!
//! This module is the one place that decides which rows render and what the
//! notes say, so the four builders agree.
use octos_core::{Message, MessageRole};

use crate::vision;
use std::collections::HashSet;

/// Preserve the last complete image-returning tool batch through text-only
/// work. Selecting arbitrary last-N files can split a reference/current pair
/// and carry unrelated full-page images into many subsequent requests.
/// The caller selects the whole visual group with a multi-file read or packet.
struct ImageGroup {
    start: usize,
    images: HashSet<String>,
}

fn retained_images(messages: &[Message]) -> ImageGroup {
    let mut active = ImageGroup {
        start: 0,
        images: HashSet::new(),
    };
    let mut batch_start = 0;
    let mut batch = HashSet::new();
    fn finish(active: &mut ImageGroup, start: usize, images: &mut HashSet<String>) {
        if !images.is_empty() && *images != active.images {
            *active = ImageGroup {
                start,
                images: std::mem::take(images),
            };
        } else {
            images.clear();
        }
    }
    for (i, message) in messages.iter().enumerate() {
        if message.role == MessageRole::Assistant
            && message.tool_calls.as_ref().is_some_and(|c| !c.is_empty())
        {
            finish(&mut active, batch_start, &mut batch);
            batch_start = i + 1;
        }
        if message.role == MessageRole::Tool {
            batch.extend(
                message
                    .media
                    .iter()
                    .filter(|p| vision::is_image(p))
                    .map(|p| vision::image_identity(p)),
            );
        }
    }
    finish(&mut active, batch_start, &mut batch);
    active
}

/// What a tool row shows the model on this request.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ToolMedia {
    /// Image paths to encode inline.
    pub images: Vec<String>,
    /// Video paths to encode inline, for the protocols that take video.
    pub videos: Vec<String>,
    /// Text appended to the tool output when something is NOT rendered:
    /// media from an earlier batch, or media this model cannot view.
    pub note: Option<String>,
}

impl ToolMedia {
    pub fn is_empty(&self) -> bool {
        self.images.is_empty() && self.videos.is_empty()
    }
}

/// Index of the assistant row whose tool calls the transcript's trailing
/// tool rows answer: the last assistant row carrying tool calls. Tool rows
/// after it are the batch the model is about to see for the first time.
pub fn current_batch_start(messages: &[Message]) -> Option<usize> {
    messages.iter().rposition(|m| {
        m.role == MessageRole::Assistant && m.tool_calls.as_ref().is_some_and(|tc| !tc.is_empty())
    })
}

/// Whether the tool row at `idx` belongs to the current batch.
pub fn is_current(messages: &[Message], idx: usize) -> bool {
    current_batch_start(messages).is_some_and(|start| idx > start)
}

fn file_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

/// The media of the tool row at `idx` to render on this request, given what
/// the model can take. `lacks_video` alone keeps the images.
pub fn for_tool_row(
    messages: &[Message],
    idx: usize,
    lacks_vision: bool,
    lacks_video: bool,
) -> ToolMedia {
    for_tool_row_with_image_history(messages, idx, lacks_vision, lacks_video, true)
}

/// Complete-group image history for providers that retain visual evidence across tool
/// batches. The normal modality guards still apply; old videos stay omitted.
pub fn for_tool_row_with_image_history(
    messages: &[Message],
    idx: usize,
    lacks_vision: bool,
    lacks_video: bool,
    retain_images: bool,
) -> ToolMedia {
    let Some(msg) = messages.get(idx) else {
        return ToolMedia::default();
    };
    if msg.role != MessageRole::Tool {
        return ToolMedia::default();
    }
    let mut images: Vec<String> = msg
        .media
        .iter()
        .filter(|p| vision::is_image(p))
        .cloned()
        .collect();
    let videos: Vec<String> = msg
        .media
        .iter()
        .filter(|p| vision::is_video(p))
        .cloned()
        .collect();
    if images.is_empty() && videos.is_empty() {
        return ToolMedia::default();
    }
    let mut retention_notes = Vec::new();
    if retain_images && !lacks_vision {
        let kept = retained_images(messages);
        // Retain the first occurrence in the current continuous image-group epoch. Re-reading
        // identical bytes must not append a second image or move the first one,
        // both of which unnecessarily invalidate the provider's cached prefix.
        // After switching away and back, attach at the NEW group, never revive
        // an ancient row and invalidate all intervening text history.
        let mut seen: HashSet<String> = messages[kept.start.min(idx)..idx]
            .iter()
            .filter(|m| m.role == MessageRole::Tool)
            .flat_map(|m| m.media.iter().map(|p| vision::image_identity(p)))
            .collect();
        images.retain(|p| {
            if idx < kept.start || !kept.images.contains(&vision::image_identity(p)) {
                retention_notes.push(format!("[image outside the active visual working set: {}. Pixels are NOT in this request; read it again for a fresh visual judgment.]", file_name(p)));
                false
            } else if !seen.insert(vision::image_identity(p)) {
                retention_notes.push(format!("[same immutable image already attached earlier in this request: {}]", file_name(p)));
                false
            } else { true }
        });
    }
    if !is_current(messages, idx) {
        if retain_images {
            let mut notes = retention_notes;
            if lacks_vision && !images.is_empty() {
                notes.push(format!(
                    "[image returned by this call cannot be shown to this model: {}. Say so; do not guess its contents. It can still be delivered as an attachment.]",
                    images.iter().map(|p| file_name(p)).collect::<Vec<_>>().join(", ")
                ));
            }
            if !videos.is_empty() {
                notes.push(format!(
                    "[media this call returned was shown to you when it ran: {}]",
                    videos
                        .iter()
                        .map(|p| file_name(p))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            return ToolMedia {
                images: if lacks_vision { Vec::new() } else { images },
                note: (!notes.is_empty()).then(|| notes.join("\n")),
                ..ToolMedia::default()
            };
        }
        let names: Vec<String> = images
            .iter()
            .chain(videos.iter())
            .map(|p| file_name(p))
            .collect();
        return ToolMedia {
            note: Some(format!(
                "[media this call returned was shown to you when it ran: {}]",
                names.join(", ")
            )),
            ..ToolMedia::default()
        };
    }
    let mut out = ToolMedia::default();
    let mut notes = retention_notes;
    if lacks_vision {
        if !images.is_empty() {
            notes.push(format!(
                "[image returned by this call cannot be shown to this model: {}. Say so; do not guess its contents. It can still be delivered as an attachment.]",
                images.iter().map(|p| file_name(p)).collect::<Vec<_>>().join(", ")
            ));
        }
    } else {
        out.images = images;
    }
    if lacks_vision || lacks_video {
        if !videos.is_empty() {
            notes.push(format!(
                "[video returned by this call cannot be viewed by this model: {}. Say so; do not guess its contents.]",
                videos.iter().map(|p| file_name(p)).collect::<Vec<_>>().join(", ")
            ));
        }
    } else {
        out.videos = videos;
    }
    if !notes.is_empty() {
        out.note = Some(notes.join("\n"));
    }
    out
}

/// The text that accompanies media rendered as a separate user turn (the
/// protocols with no media in tool outputs), naming the call it answers so
/// the model can tell a returned screenshot from a person's attachment.
pub fn shown_note(call_id: &str, paths: &[String]) -> String {
    format!(
        "[media returned by tool call {call_id}: {}]",
        paths
            .iter()
            .map(|p| file_name(p))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The note for media that was to be shown but could not be read when the
/// request was built (deleted or replaced since the tool validated it).
pub fn unreadable_note(path: &str) -> String {
    format!(
        "[media returned by this call could not be read when this request was sent: {}; not shown]",
        file_name(path)
    )
}

/// Append `note` to tool output text, on its own line.
pub fn with_note(content: &str, note: Option<&str>) -> String {
    match note {
        Some(note) if content.is_empty() => note.to_string(),
        Some(note) => format!("{content}\n{note}"),
        None => content.to_string(),
    }
}

/// Rough token cost of inline media, for the context estimator: images cost
/// roughly (w × h) / 750 on the providers that publish a formula; a phone
/// screenshot lands near this. Video is billed per second; a short clip
/// is a few thousand tokens.
pub const IMAGE_TOKEN_ESTIMATE: u32 = 1_600;
pub const VIDEO_TOKEN_ESTIMATE: u32 = 8_000;

pub fn estimate_media_tokens(paths: &[String]) -> u32 {
    paths
        .iter()
        .map(|p| {
            if vision::is_image(p) {
                IMAGE_TOKEN_ESTIMATE
            } else if vision::is_video(p) {
                VIDEO_TOKEN_ESTIMATE
            } else {
                0
            }
        })
        .sum()
}

#[cfg(test)]
mod working_set_tests {
    use super::*;
    use octos_core::ToolCall;

    fn batch(messages: &mut Vec<Message>, paths: &[&str]) {
        let id = format!("call-{}", messages.len());
        let mut assistant = Message::assistant("");
        assistant.tool_calls = Some(vec![ToolCall {
            id: id.clone(),
            name: "read_file".into(),
            arguments: serde_json::json!({}),
            metadata: None,
        }]);
        messages.push(assistant);
        let mut tool = Message::assistant("result");
        tool.role = MessageRole::Tool;
        tool.tool_call_id = Some(id);
        tool.media = paths.iter().map(|s| s.to_string()).collect();
        messages.push(tool);
    }
    fn images(messages: &[Message]) -> Vec<String> {
        (0..messages.len())
            .flat_map(|i| for_tool_row(messages, i, false, true).images)
            .collect()
    }
    #[test]
    fn default_keeps_pixels_after_text_and_reread_deduplicates_without_moving_prefix() {
        let mut m = vec![];
        batch(&mut m, &["sha-a.png"]);
        let initial = for_tool_row(&m, 1, false, true);
        batch(&mut m, &[]);
        assert_eq!(for_tool_row(&m, 1, false, true), initial);
        batch(&mut m, &["sha-a.png"]);
        assert_eq!(images(&m), ["sha-a.png"]);
        assert_eq!(for_tool_row(&m, 1, false, true), initial);
        assert!(
            for_tool_row(&m, 5, false, true)
                .note
                .unwrap()
                .contains("already attached earlier")
        );
        // A changed snapshot is different evidence, even for the same source filename.
        batch(&mut m, &["sha-b.png"]);
        assert_eq!(images(&m), ["sha-b.png"]);
    }
    #[test]
    fn group_switch_is_explicit_and_text_steps_preserve_the_whole_group() {
        let mut m = vec![];
        for i in 0..12 {
            batch(&mut m, &[&format!("sha-{i}.png")]);
        }
        assert_eq!(images(&m), ["sha-11.png"]);
        assert!(
            for_tool_row(&m, 1, false, true)
                .note
                .unwrap()
                .contains("Pixels are NOT")
        );
        batch(&mut m, &["sha-0.png"]);
        assert!(images(&m).contains(&"sha-0.png".into()));
        let names: Vec<_> = (20..32).map(|i| format!("sha-{i}.png")).collect();
        batch(
            &mut m,
            &names.iter().map(String::as_str).collect::<Vec<_>>(),
        );
        assert_eq!(images(&m), names);
        batch(&mut m, &[]);
        assert_eq!(
            images(&m),
            names,
            "text tools must not split a large comparison group"
        );
    }
    #[test]
    fn identical_snapshots_in_different_private_directories_are_one_image() {
        let a = vision::snapshot_image_bytes(b"\x89PNG\r\n\x1a\n").unwrap();
        let b = vision::snapshot_image_bytes(b"\x89PNG\r\n\x1a\n").unwrap();
        assert_ne!(a, b);
        let mut m = vec![];
        batch(&mut m, &[a.to_str().unwrap()]);
        batch(&mut m, &[b.to_str().unwrap()]);
        assert_eq!(images(&m), vec![a.to_string_lossy().into_owned()]);
    }
    #[test]
    fn revisiting_an_evicted_image_does_not_rewrite_ancient_history() {
        let mut m = vec![];
        batch(&mut m, &["a.png", "reference.png"]);
        batch(&mut m, &["b.png"]);
        let archived = for_tool_row(&m, 1, false, true);
        assert!(archived.images.is_empty());
        batch(&mut m, &[]);
        batch(&mut m, &["a.png", "reference.png"]);
        assert_eq!(for_tool_row(&m, 1, false, true), archived);
        let fresh = for_tool_row(&m, 7, false, true);
        assert_eq!(fresh.images, ["a.png", "reference.png"]);
        batch(&mut m, &[]);
        batch(&mut m, &["a.png", "reference.png"]);
        assert_eq!(for_tool_row(&m, 7, false, true), fresh);
        assert!(for_tool_row(&m, 11, false, true).images.is_empty());
    }
    #[test]
    fn modality_guards_and_explicit_opt_out_still_apply() {
        let mut m = vec![];
        batch(&mut m, &["a.png", "v.mp4"]);
        batch(&mut m, &[]);
        assert!(for_tool_row(&m, 1, true, true).images.is_empty());
        assert!(for_tool_row(&m, 1, false, false).videos.is_empty());
        assert!(
            for_tool_row_with_image_history(&m, 1, false, true, false)
                .images
                .is_empty()
        );
        let defaults: crate::openai::ModelHints = serde_json::from_str("{}").unwrap();
        assert!(defaults.retain_tool_images);
        let opt_out: crate::openai::ModelHints =
            serde_json::from_str(r#"{"retain_tool_images":false}"#).unwrap();
        assert!(!opt_out.retain_tool_images);
    }
}
