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
                "[image returned by this call cannot be shown to this model: {}. Say so; do not guess its contents.]",
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
mod tests {
    use super::*;

    fn row(role: MessageRole, media: &[&str], tool_calls: bool) -> Message {
        Message {
            role,
            content: "x".into(),
            media: media.iter().map(|s| s.to_string()).collect(),
            tool_calls: if tool_calls {
                Some(vec![octos_core::ToolCall {
                    id: "c1".into(),
                    name: "view_image".into(),
                    arguments: serde_json::json!({}),
                    metadata: None,
                }])
            } else {
                None
            },
            tool_call_id: None,
            reasoning_content: None,
            client_message_id: None,
            thread_id: None,
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn only_the_current_batch_renders_and_older_media_becomes_a_note() {
        let msgs = vec![
            row(MessageRole::User, &[], false),
            row(MessageRole::Assistant, &[], true),
            row(MessageRole::Tool, &["/t/old.png"], false),
            row(MessageRole::Assistant, &[], false),
            row(MessageRole::User, &[], false),
            row(MessageRole::Assistant, &[], true),
            row(MessageRole::Tool, &["/t/new.png", "/t/clip.mp4"], false),
        ];
        assert_eq!(current_batch_start(&msgs), Some(5));
        let old = for_tool_row(&msgs, 2, false, false);
        assert!(old.is_empty());
        assert!(old.note.as_deref().unwrap().contains("old.png"), "{old:?}");
        let new = for_tool_row(&msgs, 6, false, false);
        assert_eq!(new.images, vec!["/t/new.png"]);
        assert_eq!(new.videos, vec!["/t/clip.mp4"]);
        assert!(new.note.is_none());
    }

    #[test]
    fn a_model_without_video_keeps_the_image_and_is_told_about_the_clip() {
        let msgs = vec![
            row(MessageRole::Assistant, &[], true),
            row(MessageRole::Tool, &["/t/a.png", "/t/b.mp4"], false),
        ];
        let tm = for_tool_row(&msgs, 1, false, true);
        assert_eq!(tm.images, vec!["/t/a.png"]);
        assert!(tm.videos.is_empty());
        assert!(tm.note.as_deref().unwrap().contains("b.mp4"));
        let blind = for_tool_row(&msgs, 1, true, false);
        assert!(blind.is_empty());
        let note = blind.note.unwrap();
        assert!(note.contains("a.png") && note.contains("b.mp4"), "{note}");
    }

    #[test]
    fn media_tokens_are_counted() {
        assert_eq!(
            estimate_media_tokens(&["a.png".into(), "b.mp4".into(), "c.txt".into()]),
            IMAGE_TOKEN_ESTIMATE + VIDEO_TOKEN_ESTIMATE
        );
    }
}
