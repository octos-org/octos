//! Media a tool handed the model.
//!
//! A tool that wants the model to look at something (`view_image`,
//! `view_video`) returns paths as `ToolResult::model_media`; the agent loop
//! puts them on the tool row's `media`. Nothing else is added to the
//! transcript. Each provider then renders that media in its own correct
//! shape — Anthropic inside the `tool_result` block, Gemini as a multimodal
//! function response, the OpenAI-compatible and Responses protocols as a
//! user turn built at wire time after the batch's tool outputs — and only
//! for the tool batch the model is about to answer. An older row's media is
//! named in a note instead of being re-encoded on every request.
//!
//! This module is the one place that decides which rows render and what the
//! notes say, so the four builders agree.
use octos_core::{Message, MessageRole};

use crate::vision;

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
    for_tool_row_with_image_history(messages, idx, lacks_vision, lacks_video, false)
}

/// Opt-in image history for providers that retain visual evidence across tool
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
    let images: Vec<String> = msg
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
    if !is_current(messages, idx) {
        if retain_images {
            let mut notes = Vec::new();
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
    let mut notes = Vec::new();
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
mod tests {}
