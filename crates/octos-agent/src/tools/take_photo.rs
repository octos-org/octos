//! Take photo tool for capturing images from the MacBook camera.

use std::path::PathBuf;

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use octos_core::OutboundMessage;
use serde::Deserialize;
use tokio::sync::mpsc;

use super::{Tool, ToolResult};

/// Tool that captures a photo from the device camera and optionally sends it to chat.
pub struct TakePhotoTool {
    out_tx: mpsc::Sender<OutboundMessage>,
    default_channel: std::sync::Mutex<String>,
    default_chat_id: std::sync::Mutex<String>,
}

impl TakePhotoTool {
    pub fn new(out_tx: mpsc::Sender<OutboundMessage>) -> Self {
        Self {
            out_tx,
            default_channel: std::sync::Mutex::new(String::new()),
            default_chat_id: std::sync::Mutex::new(String::new()),
        }
    }

    /// Update the default channel/chat_id context (called per inbound message).
    pub fn set_context(&self, channel: &str, chat_id: &str) {
        *self
            .default_channel
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = channel.to_string();
        *self
            .default_chat_id
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = chat_id.to_string();
    }
}

#[derive(Deserialize)]
struct Input {
    #[serde(default)]
    confirmed: Option<bool>,
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    send: Option<bool>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    chat_id: Option<String>,
    #[serde(default)]
    device: Option<String>,
}

#[async_trait]
impl Tool for TakePhotoTool {
    fn name(&self) -> &str {
        "take_photo"
    }

    fn description(&self) -> &str {
        "Capture a photo from the device camera (e.g. MacBook FaceTime camera). \
         By default the photo is sent to the current chat. Set send=false to only \
         capture and return the file path without sending. \
         IMPORTANT: You MUST ask the user for permission before taking a photo. \
         Only call this tool with confirmed=true after the user explicitly agrees."
    }

    fn tags(&self) -> &[&str] {
        &["gateway"]
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "confirmed": {
                    "type": "boolean",
                    "description": "Must be true. You MUST ask the user for permission first and only set this to true after they agree."
                },
                "caption": {
                    "type": "string",
                    "description": "Optional caption for the photo when sending"
                },
                "send": {
                    "type": "boolean",
                    "description": "Whether to send the photo to chat (default: true)"
                },
                "channel": {
                    "type": "string",
                    "description": "Target channel. Defaults to current."
                },
                "chat_id": {
                    "type": "string",
                    "description": "Target chat/user ID. Defaults to current."
                },
                "device": {
                    "type": "string",
                    "description": "Camera device index (default: '0' for FaceTime HD Camera)"
                }
            }
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input =
            serde_json::from_value(args.clone()).wrap_err("invalid take_photo tool input")?;

        // Require explicit confirmation before accessing the camera
        if !input.confirmed.unwrap_or(false) {
            return Ok(ToolResult {
                output: "Permission required: You must ask the user for permission before \
                         taking a photo. Ask them first, then call this tool again with \
                         confirmed=true after they agree."
                    .to_string(),
                success: false,
                ..Default::default()
            });
        }

        let device = input.device.unwrap_or_else(|| "0".to_string());
        let send = input.send.unwrap_or(true);

        // Generate output path
        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let photo_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".octos")
            .join("media");
        tokio::fs::create_dir_all(&photo_dir).await.ok();
        let photo_path = photo_dir.join(format!("photo_{timestamp}.jpg"));
        let photo_path_str = photo_path.display().to_string();

        // Capture photo using ffmpeg AVFoundation. Bounded at 15s so a missing
        // permission prompt or unreachable camera device can't hang the agent
        // (or the test binary — CI runners have no camera).
        let ffmpeg_fut = tokio::process::Command::new("ffmpeg")
            .args([
                "-f",
                "avfoundation",
                "-video_size",
                "1280x720",
                "-framerate",
                "30",
                "-i",
                &device,
                "-frames:v",
                "1",
                "-y",
                &photo_path_str,
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output();

        let output =
            match tokio::time::timeout(std::time::Duration::from_secs(15), ffmpeg_fut).await {
                Ok(res) => res,
                Err(_) => {
                    return Ok(ToolResult {
                        output: "Error: ffmpeg photo capture timed out after 15s (no camera, \
                             permission prompt, or device busy)"
                            .to_string(),
                        success: false,
                        ..Default::default()
                    });
                }
            };

        match output {
            Ok(out) if out.status.success() => {
                if !photo_path.exists() {
                    return Ok(ToolResult {
                        output: "Error: ffmpeg completed but no photo file was created".into(),
                        success: false,
                        ..Default::default()
                    });
                }

                if send {
                    let channel = input.channel.unwrap_or_else(|| {
                        self.default_channel
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone()
                    });
                    let chat_id = input.chat_id.unwrap_or_else(|| {
                        self.default_chat_id
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone()
                    });

                    if channel.is_empty() || chat_id.is_empty() {
                        return Ok(ToolResult {
                            output: format!(
                                "Photo captured at {photo_path_str} but no target channel/chat \
                                 specified for sending."
                            ),
                            success: true,
                            ..Default::default()
                        });
                    }

                    let msg = OutboundMessage {
                        channel: channel.clone(),
                        chat_id: chat_id.clone(),
                        content: input.caption.unwrap_or_default(),
                        reply_to: None,
                        media: vec![photo_path_str.clone()],
                        metadata: serde_json::json!({}),
                    };

                    self.out_tx
                        .send(msg)
                        .await
                        .map_err(|e| eyre::eyre!("failed to send photo message: {e}"))?;

                    Ok(ToolResult {
                        output: format!(
                            "Photo captured and sent to {channel}:{chat_id} ({photo_path_str})"
                        ),
                        success: true,
                        ..Default::default()
                    })
                } else {
                    Ok(ToolResult {
                        output: format!("Photo captured: {photo_path_str}"),
                        success: true,
                        ..Default::default()
                    })
                }
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                Ok(ToolResult {
                    output: format!("Error: ffmpeg failed to capture photo: {stderr}"),
                    success: false,
                    ..Default::default()
                })
            }
            Err(e) => Ok(ToolResult {
                output: format!("Error: failed to run ffmpeg (is it installed?): {e}"),
                success: false,
                ..Default::default()
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_take_photo_no_ffmpeg_graceful() {
        // This test verifies the tool handles missing ffmpeg gracefully.
        // In CI or environments without ffmpeg, it should return an error, not panic.
        let (tx, _rx) = mpsc::channel(16);
        let tool = TakePhotoTool::new(tx);
        tool.set_context("telegram", "12345");

        let result = tool
            .execute(&serde_json::json!({"send": false, "confirmed": true}))
            .await
            .unwrap();

        // The result depends on whether ffmpeg is installed.
        // We just verify it doesn't panic and returns a valid ToolResult.
        assert!(result.output.contains("Photo captured") || result.output.contains("Error"));
    }

    #[tokio::test]
    async fn test_take_photo_no_target() {
        let (tx, _rx) = mpsc::channel(16);
        let tool = TakePhotoTool::new(tx);
        // No context set, send=true (default)

        // Without confirmed=true, should require permission
        let result = tool.execute(&serde_json::json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.output.contains("Permission required"));

        // With confirmed=true but no target channel
        let result = tool
            .execute(&serde_json::json!({"confirmed": true}))
            .await
            .unwrap();
        // Either ffmpeg fails (no camera in CI) or succeeds but can't send (no target)
        assert!(!result.output.is_empty());
    }
}
