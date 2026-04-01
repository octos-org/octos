//! Send file tool for delivering files to chat channels.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use eyre::{Result, WrapErr};
use octos_core::OutboundMessage;
use serde::Deserialize;
use tokio::sync::mpsc;

use super::{Tool, ToolResult};

/// Tool that sends a file to the current chat channel as a document attachment.
pub struct SendFileTool {
    out_tx: mpsc::Sender<OutboundMessage>,
    default_channel: std::sync::Mutex<String>,
    default_chat_id: std::sync::Mutex<String>,
    /// Base directory for path resolution and validation. Relative paths are
    /// resolved against this directory. File paths must resolve under this
    /// directory (prevents exfiltrating files from other profiles).
    base_dir: Option<PathBuf>,
    /// Additional allowed directories beyond base_dir (e.g. data_dir for
    /// pipeline-generated files). Absolute paths under these dirs are accepted.
    extra_allowed_dirs: Vec<PathBuf>,
}

impl SendFileTool {
    pub fn new(out_tx: mpsc::Sender<OutboundMessage>) -> Self {
        Self {
            out_tx,
            default_channel: std::sync::Mutex::new(String::new()),
            default_chat_id: std::sync::Mutex::new(String::new()),
            base_dir: None,
            extra_allowed_dirs: Vec::new(),
        }
    }

    /// Create a new SendFileTool with context pre-set (for per-session instances).
    pub fn with_context(
        out_tx: mpsc::Sender<OutboundMessage>,
        channel: impl Into<String>,
        chat_id: impl Into<String>,
    ) -> Self {
        Self {
            out_tx,
            default_channel: std::sync::Mutex::new(channel.into()),
            default_chat_id: std::sync::Mutex::new(chat_id.into()),
            base_dir: None,
            extra_allowed_dirs: Vec::new(),
        }
    }

    /// Set the base directory for file path resolution and validation.
    pub fn with_base_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.base_dir = Some(dir.into());
        self
    }

    /// Add an extra allowed directory (e.g. data_dir for pipeline-generated files).
    pub fn with_extra_allowed_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.extra_allowed_dirs.push(dir.into());
        self
    }

    /// Update the default channel/chat_id context (called per inbound message).
    /// WARNING: This mutates shared state. See MessageTool::set_context() for details.
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
    file_path: String,
    #[serde(default)]
    caption: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    chat_id: Option<String>,
}

#[async_trait]
impl Tool for SendFileTool {
    fn name(&self) -> &str {
        "send_file"
    }

    fn description(&self) -> &str {
        "Send a file to the user as a document attachment. Use this to deliver files \
         (reports, code, data, etc.) directly to the chat. The file is sent as-is, \
         not rendered as text."
    }

    fn tags(&self) -> &[&str] {
        &["gateway"]
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file to send"
                },
                "caption": {
                    "type": "string",
                    "description": "Optional caption/description for the file"
                },
                "channel": {
                    "type": "string",
                    "description": "Target channel. Defaults to current."
                },
                "chat_id": {
                    "type": "string",
                    "description": "Target chat/user ID. Defaults to current."
                }
            },
            "required": ["file_path"]
        })
    }

    async fn execute(&self, args: &serde_json::Value) -> Result<ToolResult> {
        let input: Input =
            serde_json::from_value(args.clone()).wrap_err("invalid send_file tool input")?;

        // Resolve file path: if base_dir is set, resolve relative paths against
        // it (the OS process cwd may differ from the logical working directory).
        let raw_path = Path::new(&input.file_path);
        let path = if let Some(ref base_dir) = self.base_dir {
            if raw_path.is_relative() {
                base_dir.join(raw_path)
            } else {
                raw_path.to_path_buf()
            }
        } else {
            raw_path.to_path_buf()
        };

        // Validate file path is within the allowed base directory (if set).
        // This prevents exfiltrating files from other profiles' data directories.
        // /tmp/ is always allowed since skills commonly write output there.
        if let Some(ref base_dir) = self.base_dir {
            let canonical_base =
                std::fs::canonicalize(base_dir).unwrap_or_else(|_| base_dir.clone());
            let tmp_dir = std::fs::canonicalize("/tmp").unwrap_or_else(|_| PathBuf::from("/tmp"));
            let extra_canonical: Vec<PathBuf> = self
                .extra_allowed_dirs
                .iter()
                .map(|d| std::fs::canonicalize(d).unwrap_or_else(|_| d.clone()))
                .collect();
            match std::fs::canonicalize(&path) {
                Ok(canonical_path) => {
                    let allowed = canonical_path.starts_with(&canonical_base)
                        || canonical_path.starts_with(&tmp_dir)
                        || extra_canonical
                            .iter()
                            .any(|d| canonical_path.starts_with(d));
                    if !allowed {
                        return Ok(ToolResult {
                            output: format!(
                                "Error: File path is outside the allowed directory: {}",
                                input.file_path
                            ),
                            success: false,
                            ..Default::default()
                        });
                    }
                }
                Err(_) => {
                    // Path can't be canonicalized (broken symlink, non-existent, etc.).
                    // Reject rather than silently skip the check — prevents TOCTOU bypass.
                    return Ok(ToolResult {
                        output: format!("Error: Cannot resolve file path: {}", input.file_path),
                        success: false,
                        ..Default::default()
                    });
                }
            }
        }

        // Validate file exists
        if !path.exists() {
            return Ok(ToolResult {
                output: format!("Error: File not found: {}", input.file_path),
                success: false,
                ..Default::default()
            });
        }
        if !path.is_file() {
            return Ok(ToolResult {
                output: format!("Error: Not a file: {}", input.file_path),
                success: false,
                ..Default::default()
            });
        }

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
                output: "Error: No target channel/chat specified.".into(),
                success: false,
                ..Default::default()
            });
        }

        let msg = OutboundMessage {
            channel: channel.clone(),
            chat_id: chat_id.clone(),
            content: input.caption.unwrap_or_default(),
            reply_to: None,
            media: vec![path.to_string_lossy().into_owned()],
            metadata: serde_json::json!({}),
        };

        self.out_tx
            .send(msg)
            .await
            .map_err(|e| eyre::eyre!("failed to send file message: {e}"))?;

        let filename = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| input.file_path.clone());

        Ok(ToolResult {
            output: format!("File '{filename}' sent to {channel}:{chat_id}"),
            success: true,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn test_send_file() {
        let (tx, mut rx) = mpsc::channel(16);
        let tool = SendFileTool::new(tx);
        tool.set_context("telegram", "12345");

        // Create a temp file
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "hello world").unwrap();
        let path = tmp.path().to_string_lossy().to_string();

        let result = tool
            .execute(&serde_json::json!({
                "file_path": path,
                "caption": "Here is the file"
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert!(result.output.contains("sent to telegram:12345"));

        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.channel, "telegram");
        assert_eq!(msg.chat_id, "12345");
        assert_eq!(msg.content, "Here is the file");
        assert_eq!(msg.media.len(), 1);
        assert_eq!(msg.media[0], path);
    }

    #[tokio::test]
    async fn test_file_not_found() {
        let (tx, _rx) = mpsc::channel(16);
        let tool = SendFileTool::new(tx);
        tool.set_context("telegram", "12345");

        let result = tool
            .execute(&serde_json::json!({
                "file_path": "/nonexistent/file.txt"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("not found"));
    }

    #[tokio::test]
    async fn test_with_context_routes_correctly() {
        let (tx, mut rx) = mpsc::channel(16);
        let tool = SendFileTool::with_context(tx, "feishu", "ctx-chat");

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "data").unwrap();
        let path = tmp.path().to_string_lossy().to_string();

        let result = tool
            .execute(&serde_json::json!({"file_path": path}))
            .await
            .unwrap();

        assert!(result.success);
        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.channel, "feishu");
        assert_eq!(msg.chat_id, "ctx-chat");
    }

    #[tokio::test]
    async fn test_no_target() {
        let (tx, _rx) = mpsc::channel(16);
        let tool = SendFileTool::new(tx);
        // No context set

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "data").unwrap();

        let result = tool
            .execute(&serde_json::json!({
                "file_path": tmp.path().to_string_lossy().to_string()
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("No target"));
    }

    #[tokio::test]
    async fn test_base_dir_blocks_outside_path() {
        // Use a path under home dir (not /tmp/) to ensure the test is
        // platform-independent (tempdir may be under /tmp/ on Linux).
        let root = std::env::temp_dir().join("octos-test-send-file");
        let base = root.join("allowed");
        let outside_dir = root.join("forbidden");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&outside_dir).unwrap();

        let outside_file = outside_dir.join("secret.txt");
        std::fs::write(&outside_file, "secret data").unwrap();

        let (tx, _rx) = mpsc::channel(16);
        let tool = SendFileTool::with_context(tx, "telegram", "12345").with_base_dir(&base);

        let result = tool
            .execute(&serde_json::json!({
                "file_path": outside_file.to_string_lossy().to_string()
            }))
            .await
            .unwrap();

        // On macOS, temp_dir is /var/folders/... (not under /tmp/), so blocked.
        // On Linux, temp_dir is /tmp/, so the file IS under /tmp/ and allowed.
        // Test the correct platform behavior:
        let canonical_tmp =
            std::fs::canonicalize("/tmp").unwrap_or_else(|_| std::path::PathBuf::from("/tmp"));
        let canonical_file = std::fs::canonicalize(&outside_file).unwrap();
        if canonical_file.starts_with(&canonical_tmp) {
            // Linux: file is under /tmp/ → allowed
            assert!(result.success, "file under /tmp/ should be allowed");
        } else {
            // macOS: file is NOT under /tmp/ → blocked
            assert!(
                !result.success,
                "file outside base_dir and /tmp/ should be blocked"
            );
            assert!(result.output.contains("outside the allowed directory"));
        }

        // Cleanup
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn test_base_dir_blocks_non_tmp_outside_path() {
        let base = tempfile::tempdir().unwrap();

        let (tx, _rx) = mpsc::channel(16);
        let tool = SendFileTool::with_context(tx, "telegram", "12345").with_base_dir(base.path());

        // /etc/hosts exists on all Unix systems and is outside both base_dir and /tmp/
        let test_path = if std::path::Path::new("/etc/hosts").exists() {
            "/etc/hosts"
        } else {
            "/etc/resolv.conf"
        };

        let result = tool
            .execute(&serde_json::json!({
                "file_path": test_path
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("outside the allowed directory"));
    }

    #[tokio::test]
    async fn test_base_dir_allows_inside_path() {
        let base = tempfile::tempdir().unwrap();
        let inside_file = base.path().join("report.pdf");
        std::fs::write(&inside_file, "report content").unwrap();

        let (tx, mut rx) = mpsc::channel(16);
        let tool = SendFileTool::with_context(tx, "telegram", "12345").with_base_dir(base.path());

        let result = tool
            .execute(&serde_json::json!({
                "file_path": inside_file.to_string_lossy().to_string()
            }))
            .await
            .unwrap();

        assert!(result.success);
        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.media.len(), 1);
    }

    #[tokio::test]
    async fn test_base_dir_blocks_nonexistent_path() {
        // When base_dir is set, non-existent paths should be rejected
        // (not silently bypassed via canonicalize failure)
        let base = tempfile::tempdir().unwrap();

        let (tx, _rx) = mpsc::channel(16);
        let tool = SendFileTool::with_context(tx, "telegram", "12345").with_base_dir(base.path());

        let result = tool
            .execute(&serde_json::json!({
                "file_path": "/tmp/nonexistent-secret-file.txt"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("Cannot resolve file path"));
    }

    #[tokio::test]
    async fn test_base_dir_resolves_relative_path() {
        // Relative paths should be resolved against base_dir, not OS cwd
        let base = tempfile::tempdir().unwrap();
        let sub = base.path().join("skill-output");
        std::fs::create_dir_all(&sub).unwrap();
        let file = sub.join("deck.pptx");
        std::fs::write(&file, "pptx data").unwrap();

        let (tx, mut rx) = mpsc::channel(16);
        let tool = SendFileTool::with_context(tx, "telegram", "12345").with_base_dir(base.path());

        // Pass relative path — should resolve to base_dir/skill-output/deck.pptx
        let result = tool
            .execute(&serde_json::json!({
                "file_path": "skill-output/deck.pptx"
            }))
            .await
            .unwrap();

        assert!(
            result.success,
            "relative path inside base_dir should succeed: {}",
            result.output
        );
        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.media.len(), 1);
        // The media path should be the resolved absolute path
        assert!(
            msg.media[0].contains("skill-output/deck.pptx"),
            "media path should contain resolved path: {}",
            msg.media[0]
        );
    }

    #[tokio::test]
    async fn test_base_dir_blocks_traversal() {
        let base = tempfile::tempdir().unwrap();

        let (tx, _rx) = mpsc::channel(16);
        let tool = SendFileTool::with_context(tx, "telegram", "12345").with_base_dir(base.path());

        // Try path traversal to /etc/hostname (outside both base_dir and /tmp/)
        let traversal = format!("{}/../../../etc/hostname", base.path().display());
        let result = tool
            .execute(&serde_json::json!({"file_path": traversal}))
            .await
            .unwrap();

        assert!(!result.success);
    }
}
