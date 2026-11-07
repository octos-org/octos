//! Send file tool for delivering files to chat channels.

use std::path::{Component, Path, PathBuf};

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
    default_topic: std::sync::Mutex<Option<String>>,
    /// Base directory for path resolution and validation. Relative paths are
    /// resolved against this directory. File paths must resolve under this
    /// directory (prevents exfiltrating files from other profiles).
    base_dir: Option<PathBuf>,
    /// Additional allowed directories beyond base_dir (e.g. data_dir for
    /// pipeline-generated files). Absolute paths under these dirs are accepted.
    extra_allowed_dirs: Vec<PathBuf>,
    /// Only the gateway API consumer rejects messages without a turn thread.
    /// Serve's API consumers retain their existing thread-optional behavior.
    require_turn_scope: bool,
}

fn is_stale_slides_backup(path: &Path) -> bool {
    let mut saw_slides = false;
    for component in path.components() {
        let Component::Normal(segment) = component else {
            continue;
        };
        let name = segment.to_string_lossy();
        if name == "slides" {
            saw_slides = true;
            continue;
        }
        if saw_slides && name == "output_old" {
            return true;
        }
    }
    false
}

impl SendFileTool {
    pub fn new(out_tx: mpsc::Sender<OutboundMessage>) -> Self {
        Self {
            out_tx,
            default_channel: std::sync::Mutex::new(String::new()),
            default_chat_id: std::sync::Mutex::new(String::new()),
            default_topic: std::sync::Mutex::new(None),
            base_dir: None,
            extra_allowed_dirs: Vec::new(),
            require_turn_scope: false,
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
            default_topic: std::sync::Mutex::new(None),
            base_dir: None,
            extra_allowed_dirs: Vec::new(),
            require_turn_scope: false,
        }
    }

    /// Require a real turn thread for API delivery by the gateway consumer.
    pub fn with_required_turn_scope(mut self) -> Self {
        self.require_turn_scope = true;
        self
    }

    /// Set the default topic context for topic-scoped API sessions.
    pub fn with_topic(self, topic: Option<impl Into<String>>) -> Self {
        *self.default_topic.lock().unwrap_or_else(|e| e.into_inner()) = topic.map(Into::into);
        self
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
    /// Tool call ID from the originating LLM request. Threaded through to SSE
    /// file events so the web client can link delivered files to their source message.
    #[serde(default)]
    tool_call_id: Option<String>,
    #[serde(default)]
    topic: Option<String>,
}

tokio::task_local! {
    /// M10 Phase 5a: scoped flag that marks an in-flight `send_file`
    /// invocation as a per-file companion to a `spawn_only` completion's
    /// canonical v2 background-child envelope. When the scope is active, the
    /// emitted [`OutboundMessage`] carries
    /// `metadata.spawn_complete_companion = true`, which the api/serve
    /// `send_file` consumer reads to persist the row as transcript-only.
    /// Its UI projection is suppressed in favour of the single child envelope.
    ///
    /// Internal-only by construction: the flag is NEVER read from tool
    /// `args`, so an LLM cannot inject it from a generated JSON payload.
    /// Only [`execution.rs`]'s `NotConfigured` success branch enters this
    /// scope around its retry-loop calls into `send_file`.
    static SPAWN_COMPLETE_COMPANION_SCOPE: bool;
    /// Per-invocation turn scope used only for API delivery. It comes from the
    /// typed ToolContext reporter, never from generated tool arguments.
    static SEND_FILE_THREAD_ID: Option<String>;
}

/// Run `fut` with the spawn-complete-companion scope active, so any
/// `send_file` calls executed inside its task tree set the
/// `spawn_complete_companion` outbound metadata. Use exclusively from
/// the agent-internal NotConfigured-branch retry loop in
/// `execution.rs`. `pub(crate)` so only the agent crate can enter the
/// scope; not part of the tool's external surface.
pub(crate) async fn with_spawn_complete_companion_scope<F, T>(fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    SPAWN_COMPLETE_COMPANION_SCOPE.scope(true, fut).await
}

fn current_spawn_complete_companion() -> bool {
    SPAWN_COMPLETE_COMPANION_SCOPE
        .try_with(|v| *v)
        .ok()
        .unwrap_or(false)
}

fn current_send_file_thread_id() -> Option<String> {
    SEND_FILE_THREAD_ID.try_with(Clone::clone).ok().flatten()
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

        // Step 1: when a `base_dir` is configured (the only path that
        // applies any policy today), route handle-shaped inputs
        // (`up/...`, `pf/...`) and absolute paths through the unified
        // resolver. Relative `file_path` inputs MUST stay anchored to
        // `base_dir` — codex P2 review (2026-05-13) flagged that
        // letting the resolver run on every shape lets a same-basename
        // upload (e.g. `report.pdf` written by a different session)
        // shadow the outbound workspace file, which breaks the
        // common write-then-send flow and risks cross-session leaks.
        //
        // Without `base_dir` (legacy `SendFileTool::new` path used by
        // unit tests and standalone scripts), we keep the historical
        // pass-through behaviour — no path mangling, no policy.
        let raw_path = Path::new(&input.file_path);
        let looks_like_handle =
            input.file_path.starts_with("up/") || input.file_path.starts_with("pf/");
        let use_unified_resolver = looks_like_handle || raw_path.is_absolute();

        let path = if let Some(ref base_dir) = self.base_dir {
            if use_unified_resolver {
                // Pick the first extra_allowed_dir as the profile root
                // hint. Subsequent extras still get covered by the
                // legacy allowlist below.
                let profile_hint = self.extra_allowed_dirs.first().map(PathBuf::as_path);
                match octos_core::file_handle::resolve_tool_path(
                    base_dir,
                    profile_hint,
                    &input.file_path,
                ) {
                    Ok(resolved) => resolved.absolute,
                    Err(_) => raw_path.to_path_buf(),
                }
            } else {
                // Relative outbound paths — anchor to base_dir, NOT
                // through the unified resolver (which would prefer
                // `temp_upload_root()/<basename>` on a same-name
                // collision).
                base_dir.join(raw_path)
            }
        } else {
            raw_path.to_path_buf()
        };

        // Step 2: containment check against the send_file allowlist
        // (base_dir + managed upload root + extra_allowed_dirs). The unified
        // resolver already verified containment for `up/...`/`pf/...` handles
        // and workspace-/profile-internal absolute paths, but the allowlist
        // is broader so we keep this check unconditionally.
        //
        // Plain /tmp is deliberately NOT on the allowlist (#2543-②): it is
        // world-writable, and delivery is itself an authorization act — an
        // injected instruction can plant a file there and send_file would
        // turn it into a persistent browser-downloadable copy outside the
        // workspace. The sanctioned channel for generated media is the
        // agent-owned managed uploads root (private per #2835); pipeline
        // outputs ride extra_allowed_dirs.
        if let Some(ref base_dir) = self.base_dir {
            let canonical_base =
                std::fs::canonicalize(base_dir).unwrap_or_else(|_| base_dir.clone());
            let upload_root = std::fs::canonicalize(octos_core::file_handle::temp_upload_root())
                .unwrap_or_else(|_| octos_core::file_handle::temp_upload_root());
            let extra_canonical: Vec<PathBuf> = self
                .extra_allowed_dirs
                .iter()
                .map(|d| std::fs::canonicalize(d).unwrap_or_else(|_| d.clone()))
                .collect();
            match std::fs::canonicalize(&path) {
                Ok(canonical_path) => {
                    let allowed = canonical_path.starts_with(&canonical_base)
                        || canonical_path.starts_with(&upload_root)
                        || extra_canonical
                            .iter()
                            .any(|d| canonical_path.starts_with(d));
                    if !allowed {
                        let mut output = format!(
                            "Error: File path is outside the allowed directory: {}",
                            input.file_path
                        );
                        if canonical_path.starts_with("/tmp") {
                            output.push_str(
                                " (plain /tmp is not deliverable — write outputs to the \
                                 workspace or the managed uploads directory)",
                            );
                        }
                        return Ok(ToolResult {
                            output,
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

        if is_stale_slides_backup(&path) {
            return Ok(ToolResult {
                output: format!(
                    "Error: Refusing to send stale slides backup artifact: {}",
                    input.file_path
                ),
                success: false,
                ..Default::default()
            });
        }

        let explicit_channel = input.channel.clone();
        let explicit_topic = input.topic.clone();
        let channel = input.channel.unwrap_or_else(|| {
            self.default_channel
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        });
        let explicit_chat_id = input.chat_id;
        let chat_id = explicit_chat_id.clone().unwrap_or_else(|| {
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

        if channel == "api" && explicit_chat_id.is_some() {
            let bound_chat_id = self
                .default_chat_id
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if chat_id != bound_chat_id {
                return Ok(ToolResult {
                    output: format!(
                        "Error: send_file chat_id must match the current API turn ({bound_chat_id})"
                    ),
                    success: false,
                    ..Default::default()
                });
            }
        }

        // T-0247 P2: model-supplied routing must not redirect the attachment
        // away from the current turn. When a turn context is bound, an explicit
        // `channel` / `topic` that disagrees with it fails explicitly instead
        // of reporting "File sent" while delivering elsewhere (or nowhere).
        let bound_channel = self
            .default_channel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let bound_topic = self
            .default_topic
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let context_bound = !bound_channel.is_empty() || !chat_id.is_empty();
        if context_bound {
            if let Some(ref requested) = explicit_channel
                && !bound_channel.is_empty()
                && *requested != bound_channel
            {
                return Ok(ToolResult {
                    output: format!(
                        "Error: send_file channel must match the current turn ({bound_channel}), not {requested}"
                    ),
                    success: false,
                    ..Default::default()
                });
            }
            if explicit_topic.is_some() && explicit_topic != bound_topic {
                return Ok(ToolResult {
                    output: "Error: send_file topic must match the current turn's topic;                              refusing to deliver the attachment to a different topic/session"
                        .into(),
                    success: false,
                    ..Default::default()
                });
            }
        }

        let thread_id = current_send_file_thread_id();
        // Spawn-complete companions retain their existing committed background
        // identity path. Only the gateway consumer requires a turn thread;
        // serve's API delivery remains thread-optional.
        if self.require_turn_scope
            && channel == "api"
            && thread_id.is_none()
            && !current_spawn_complete_companion()
        {
            return Ok(ToolResult {
                output: "Error: send_file requires a turn thread_id for API delivery".into(),
                success: false,
                ..Default::default()
            });
        }

        let mut metadata = serde_json::Map::new();
        if let Some(thread_id) = thread_id {
            metadata.insert(
                "thread_id".to_string(),
                serde_json::Value::String(thread_id),
            );
        }
        if let Some(ref tc_id) = input.tool_call_id {
            metadata.insert(
                "tool_call_id".to_string(),
                serde_json::Value::String(tc_id.clone()),
            );
        }
        let topic = input.topic.or_else(|| {
            self.default_topic
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        });
        if let Some(topic) = topic {
            metadata.insert("topic".to_string(), serde_json::Value::String(topic));
        }
        // M10 Phase 5a: read the task-local scope (NOT the args) to
        // decide whether this invocation is a `spawn_complete_companion`.
        // Scope-only by design — see [`SPAWN_COMPLETE_COMPANION_SCOPE`]
        // — keeps the LLM unable to spoof the flag through generated
        // tool args.
        if current_spawn_complete_companion() {
            metadata.insert(
                "spawn_complete_companion".to_string(),
                serde_json::Value::Bool(true),
            );
        }

        let msg = OutboundMessage {
            channel: channel.clone(),
            chat_id: chat_id.clone(),
            content: input.caption.unwrap_or_default(),
            reply_to: None,
            media: vec![path.to_string_lossy().into_owned()],
            metadata: serde_json::Value::Object(metadata),
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

    async fn execute_with_context(
        &self,
        ctx: &super::ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult> {
        let thread_id = ctx.reporter.thread_id().map(str::to_string);
        SEND_FILE_THREAD_ID
            .scope(thread_id, self.execute(args))
            .await
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

    /// T-0247 P2: a model-supplied `topic` must not redirect the attachment
    /// into another session's topic; a mismatch fails explicitly and delivers
    /// nothing, while the matching topic delivers exactly once.
    #[tokio::test]
    async fn send_file_rejects_model_topic_redirect_to_another_session() {
        let (tx, mut rx) = mpsc::channel(16);
        let tool = SendFileTool::new(tx).with_topic(Some("current-topic".to_string()));
        tool.set_context("api", "current-chat");
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "hello").unwrap();
        let path = tmp.path().to_string_lossy().to_string();

        let result = tool
            .execute(&serde_json::json!({
                "file_path": path,
                "topic": "other-session-topic"
            }))
            .await
            .unwrap();
        assert!(
            !result.success,
            "topic redirect must fail: {}",
            result.output
        );
        assert!(
            result.output.contains("topic"),
            "explicit reason: {}",
            result.output
        );
        assert!(
            rx.try_recv().is_err(),
            "nothing delivered on a topic redirect"
        );

        // The matching topic is delivered exactly once with the real topic.
        let ok = tool
            .execute(&serde_json::json!({
                "file_path": path,
                "topic": "current-topic"
            }))
            .await
            .unwrap();
        assert!(ok.success, "{}", ok.output);
        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.metadata["topic"], "current-topic");
        assert!(rx.try_recv().is_err(), "exactly one delivery");
    }

    /// T-0247 P2: a model-supplied `channel` that mismatches the bound turn
    /// must fail explicitly (no "File sent" with no/misdelivery).
    #[tokio::test]
    async fn send_file_rejects_model_channel_mismatch() {
        let (tx, mut rx) = mpsc::channel(16);
        let tool = SendFileTool::new(tx);
        tool.set_context("api", "current-chat");
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "hello").unwrap();
        let path = tmp.path().to_string_lossy().to_string();

        let result = tool
            .execute(&serde_json::json!({
                "file_path": path,
                "channel": "telegram"
            }))
            .await
            .unwrap();
        assert!(
            !result.success,
            "channel mismatch must fail: {}",
            result.output
        );
        assert!(
            result.output.contains("channel"),
            "explicit reason: {}",
            result.output
        );
        assert!(
            rx.try_recv().is_err(),
            "nothing delivered on a channel mismatch"
        );

        // Omitting channel (or matching it) delivers to the bound channel.
        let ok = tool
            .execute(&serde_json::json!({ "file_path": path }))
            .await
            .unwrap();
        assert!(ok.success, "{}", ok.output);
        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.channel, "api");
    }

    struct TestThreadReporter(&'static str);

    impl crate::progress::ProgressReporter for TestThreadReporter {
        fn report(&self, _event: crate::progress::ProgressEvent) {}

        fn thread_id(&self) -> Option<&str> {
            Some(self.0)
        }
    }

    async fn execute_with_test_scope(
        tool: &SendFileTool,
        args: &serde_json::Value,
    ) -> Result<ToolResult> {
        let mut ctx = crate::tools::ToolContext::zero();
        ctx.reporter = std::sync::Arc::new(TestThreadReporter("test-thread"));
        tool.execute_with_context(&ctx, args).await
    }

    #[tokio::test]
    async fn send_file_uses_each_turn_thread_id_and_rejects_missing_api_scope() {
        let (tx, mut rx) = mpsc::channel(16);
        let tool = SendFileTool::new(tx).with_required_turn_scope();
        tool.set_context("api", "chat");
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "hello").unwrap();
        let path = tmp.path().to_string_lossy().to_string();

        // A rejected attempt must not claim success or enqueue a message; the
        // same tool can then retry with the real per-turn scope.
        let missing = tool
            .execute(&serde_json::json!({"file_path": path}))
            .await
            .unwrap();
        assert!(!missing.success, "API delivery without a turn must fail");
        assert!(missing.output.contains("thread_id"));
        assert!(rx.try_recv().is_err(), "rejected scope must not send");

        for thread_id in ["cmid-turn-one", "cmid-turn-two"] {
            let mut ctx = super::super::ToolContext::zero();
            ctx.reporter = std::sync::Arc::new(TestThreadReporter(match thread_id {
                "cmid-turn-one" => "cmid-turn-one",
                _ => "cmid-turn-two",
            }));
            let result = tool
                .execute_with_context(&ctx, &serde_json::json!({"file_path": path}))
                .await
                .unwrap();
            assert!(
                result.success,
                "real turn scope must deliver: {}",
                result.output
            );
            let msg = rx.recv().await.unwrap();
            assert_eq!(msg.metadata["thread_id"], thread_id);
        }
    }

    #[tokio::test]
    async fn send_file_rejects_cross_session_api_chat_id() {
        let (tx, mut rx) = mpsc::channel(16);
        let tool = SendFileTool::new(tx).with_required_turn_scope();
        tool.set_context("api", "current-chat");
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "private").unwrap();
        let path = tmp.path().to_string_lossy().to_string();

        let rejected = execute_with_test_scope(
            &tool,
            &serde_json::json!({"file_path": path, "chat_id": "other-chat"}),
        )
        .await
        .unwrap();
        assert!(!rejected.success);
        assert!(rejected.output.contains("current API turn"));
        assert!(
            rx.try_recv().is_err(),
            "cross-session send must not enqueue"
        );

        let accepted = execute_with_test_scope(
            &tool,
            &serde_json::json!({"file_path": path, "chat_id": "current-chat"}),
        )
        .await
        .unwrap();
        assert!(accepted.success, "same-turn target must remain valid");
        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.chat_id, "current-chat");
        assert_eq!(msg.metadata["thread_id"], "test-thread");
    }

    #[tokio::test]
    async fn send_file_without_turn_scope_preserves_serve_api_delivery() {
        let (tx, mut rx) = mpsc::channel(16);
        let tool = SendFileTool::new(tx);
        tool.set_context("api", "serve-chat");
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "serve").unwrap();
        let path = tmp.path().to_string_lossy().to_string();

        let result = tool
            .execute_with_context(
                &super::super::ToolContext::zero(),
                &serde_json::json!({"file_path": path}),
            )
            .await
            .unwrap();
        assert!(result.success, "serve API delivery is thread-optional");
        let msg = rx.recv().await.unwrap();
        assert!(msg.metadata.get("thread_id").is_none());
        assert_eq!(msg.media.len(), 1);
    }

    // #2543-②: delivery is itself an authorization act. Plain /tmp is
    // world-writable, so a prompt-injected shell step can plant a file there
    // and send_file would turn it into a persistent browser-downloadable
    // copy outside the workspace. The managed uploads root stays allowed;
    // bare /tmp must refuse.
    #[tokio::test]
    async fn test_send_file_refuses_plain_tmp_outside_workspace() {
        let (tx, _rx) = mpsc::channel(16);
        let ws = tempfile::tempdir().unwrap();
        let tool = SendFileTool::new(tx).with_base_dir(ws.path());
        tool.set_context("telegram", "12345");

        let planted = tempfile::NamedTempFile::new().unwrap();
        let path = planted.path().to_string_lossy().to_string();

        let result = tool
            .execute(&serde_json::json!({
                "file_path": path,
                "caption": "planted"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("outside the allowed directory"));
    }

    #[tokio::test]
    async fn test_send_file_allows_managed_upload_root() {
        let (tx, mut rx) = mpsc::channel(16);
        let ws = tempfile::tempdir().unwrap();
        let tool = SendFileTool::new(tx).with_base_dir(ws.path());
        tool.set_context("telegram", "12345");

        let root = octos_core::file_handle::temp_upload_root();
        std::fs::create_dir_all(&root).unwrap();
        let managed = root.join("send-file-managed-test.txt");
        std::fs::write(&managed, "managed output").unwrap();

        let result = tool
            .execute(&serde_json::json!({
                "file_path": managed.to_string_lossy().to_string()
            }))
            .await
            .unwrap();

        assert!(result.success, "managed upload root must stay deliverable");
        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.media.len(), 1);
        let _ = std::fs::remove_file(&managed);
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
        // #2543-②: blocked unconditionally now — bare /tmp left the allowlist,
        // so the old "allowed on Linux because it's under /tmp" branch is gone.
        assert!(
            !result.success,
            "file outside base_dir must be blocked (including under plain /tmp)"
        );
        assert!(result.output.contains("outside the allowed directory"));

        // Cleanup
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    #[cfg_attr(target_os = "windows", ignore)]
    async fn test_base_dir_blocks_non_tmp_outside_path() {
        let base = tempfile::tempdir().unwrap();

        // Create a file outside base_dir and outside /tmp/, since /tmp/ is
        // explicitly allowlisted for generated artifacts. Anchor the "outside"
        // dir under the user's home directory so it's stable regardless of
        // whether the worktree itself lives under /tmp/ (e.g. CI scratch dirs).
        let canonical_tmp =
            std::fs::canonicalize("/tmp").unwrap_or_else(|_| std::path::PathBuf::from("/tmp"));
        let Some(home) = dirs::home_dir() else {
            eprintln!("skipping test_base_dir_blocks_non_tmp_outside_path: no home dir");
            return;
        };
        let canonical_home = std::fs::canonicalize(&home).unwrap_or(home);
        if canonical_home.starts_with(&canonical_tmp) {
            eprintln!(
                "skipping test_base_dir_blocks_non_tmp_outside_path: $HOME is under /tmp/, no stable non-tmp location available"
            );
            return;
        }
        let outside_dir = tempfile::Builder::new()
            .prefix("octos-send-file-outside-")
            .tempdir_in(&canonical_home)
            .unwrap();
        let outside_file = outside_dir.path().join("secret.txt");
        std::fs::write(&outside_file, "secret").unwrap();

        let (tx, _rx) = mpsc::channel(16);
        let tool = SendFileTool::with_context(tx, "telegram", "12345").with_base_dir(base.path());

        let result = tool
            .execute(&serde_json::json!({
                "file_path": outside_file.to_string_lossy()
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(
            result.output.contains("outside the allowed directory"),
            "expected 'outside the allowed directory', got: {}",
            result.output
        );
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

    #[tokio::test]
    async fn should_include_tool_call_id_in_metadata_when_provided() {
        let (tx, mut rx) = mpsc::channel(16);
        let tool = SendFileTool::with_context(tx, "api", "sess-1");

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "audio data").unwrap();
        let path = tmp.path().to_string_lossy().to_string();

        let result = execute_with_test_scope(
            &tool,
            &serde_json::json!({
                "file_path": path,
                "tool_call_id": "call_abc123"
            }),
        )
        .await
        .unwrap();

        assert!(result.success);
        let msg = rx.recv().await.unwrap();
        assert_eq!(
            msg.metadata.get("tool_call_id").and_then(|v| v.as_str()),
            Some("call_abc123"),
        );
    }

    #[tokio::test]
    async fn should_omit_tool_call_id_from_metadata_when_absent() {
        let (tx, mut rx) = mpsc::channel(16);
        let tool = SendFileTool::with_context(tx, "api", "sess-1");

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "data").unwrap();
        let path = tmp.path().to_string_lossy().to_string();

        let result = execute_with_test_scope(&tool, &serde_json::json!({"file_path": path}))
            .await
            .unwrap();

        assert!(result.success);
        let msg = rx.recv().await.unwrap();
        assert!(msg.metadata.get("tool_call_id").is_none());
    }

    #[tokio::test]
    async fn should_set_spawn_complete_companion_metadata_when_scope_active() {
        // M10 Phase 5a: when execution.rs's NotConfigured success branch
        // wraps the per-file `send_file` retry loop in
        // `with_spawn_complete_companion_scope`, the in-flight execute()
        // call sees the task-local flag and stamps the OutboundMessage's
        // metadata with `spawn_complete_companion: true`. The api/serve
        // consumer reads the flag and persists the resulting row as
        // transcript-only, suppressing its UI projection in favour of the
        // canonical background-child envelope.
        let (tx, mut rx) = mpsc::channel(16);
        let tool = SendFileTool::with_context(tx, "api", "sess-1");

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "report").unwrap();
        let path = tmp.path().to_string_lossy().to_string();

        // Wrapping in the scope is the ONLY way to set the flag — the
        // input schema deliberately does not accept it from JSON args, so
        // an LLM cannot spoof the marker through generated tool calls.
        let result = with_spawn_complete_companion_scope(execute_with_test_scope(
            &tool,
            &serde_json::json!({"file_path": path}),
        ))
        .await
        .unwrap();

        assert!(result.success);
        let msg = rx.recv().await.unwrap();
        assert_eq!(
            msg.metadata
                .get("spawn_complete_companion")
                .and_then(|v| v.as_bool()),
            Some(true),
        );
        assert_eq!(msg.metadata["thread_id"], "test-thread");
    }

    #[tokio::test]
    async fn should_omit_spawn_complete_companion_metadata_outside_scope() {
        // The flag is scope-driven. A regular agent-issued `send_file` call
        // (LLM tool-use, explicit user request) runs OUTSIDE the
        // companion scope, so the metadata key must stay absent and the
        // resulting row reaches all clients with `source: Assistant`.
        let (tx, mut rx) = mpsc::channel(16);
        let tool = SendFileTool::with_context(tx, "api", "sess-1");

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "data").unwrap();
        let path = tmp.path().to_string_lossy().to_string();

        let result = execute_with_test_scope(&tool, &serde_json::json!({"file_path": path}))
            .await
            .unwrap();

        assert!(result.success);
        let msg = rx.recv().await.unwrap();
        assert!(msg.metadata.get("spawn_complete_companion").is_none());
    }

    #[tokio::test]
    async fn should_ignore_spawn_complete_companion_field_in_args() {
        // Defense-in-depth: even if a malicious or buggy caller puts an
        // `_spawn_complete_companion` key in the JSON args, the field is
        // not part of the tool's `Input` shape (no struct field deserializes
        // it). The tool ignores it and decides solely from the task-local
        // scope. The metadata flag stays absent — preventing an LLM from
        // spoofing the Background-source filter through generated args.
        let (tx, mut rx) = mpsc::channel(16);
        let tool = SendFileTool::with_context(tx, "api", "sess-1");

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "data").unwrap();
        let path = tmp.path().to_string_lossy().to_string();

        let result = execute_with_test_scope(
            &tool,
            &serde_json::json!({
                "file_path": path,
                "_spawn_complete_companion": true,
                "spawn_complete_companion": true
            }),
        )
        .await
        .unwrap();

        assert!(result.success);
        let msg = rx.recv().await.unwrap();
        assert!(
            msg.metadata.get("spawn_complete_companion").is_none(),
            "args-passed companion flag must be ignored — only the task-local scope can mark a row",
        );
    }

    #[tokio::test]
    async fn should_include_default_topic_in_metadata_when_configured() {
        let (tx, mut rx) = mpsc::channel(16);
        let tool = SendFileTool::with_context(tx, "api", "sess-1").with_topic(Some("slides demo"));

        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "pptx data").unwrap();
        let path = tmp.path().to_string_lossy().to_string();

        let result = execute_with_test_scope(
            &tool,
            &serde_json::json!({
                "file_path": path,
                "caption": "deck"
            }),
        )
        .await
        .unwrap();

        assert!(result.success);
        let msg = rx.recv().await.unwrap();
        assert_eq!(
            msg.metadata.get("topic").and_then(|v| v.as_str()),
            Some("slides demo"),
        );
    }

    #[test]
    fn stale_slides_backup_detection_matches_output_old_paths() {
        assert!(is_stale_slides_backup(Path::new(
            "/tmp/workspace/slides/demo/output_old/deck.pptx"
        )));
        assert!(!is_stale_slides_backup(Path::new(
            "/tmp/workspace/slides/demo/output/deck.pptx"
        )));
    }

    /// Codex review P2 (2026-05-13): a relative `file_path` like
    /// `report.pdf` MUST resolve under `base_dir`, not against the
    /// global upload tmpdir. Letting the unified resolver run on every
    /// shape allowed a same-name upload (potentially from another
    /// session) to shadow the outbound workspace file.
    #[tokio::test]
    async fn relative_path_prefers_base_dir_over_upload_tmpdir_collision() {
        let base = tempfile::tempdir().unwrap();
        let intended = base.path().join("report.pdf");
        std::fs::write(&intended, b"workspace-report").unwrap();

        // Plant a same-basename file under the upload tmpdir. The
        // resolver's bare-basename branch (step 4 of `resolve_tool_path`)
        // would prefer this over the workspace file if we let it run.
        let upload_root = octos_core::file_handle::temp_upload_root();
        std::fs::create_dir_all(&upload_root).unwrap();
        let upload_name = format!(
            "report-{}-{}.pdf",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        // Use a unique upload name so we can also test the bare-basename
        // collision shape explicitly without polluting other tests.
        let upload_clone = upload_root.join("report.pdf");
        let _ = std::fs::write(&upload_clone, b"OTHER-SESSION-UPLOAD");

        let (tx, mut rx) = mpsc::channel(16);
        let tool = SendFileTool::with_context(tx, "telegram", "12345").with_base_dir(base.path());

        let result = tool
            .execute(&serde_json::json!({"file_path": "report.pdf"}))
            .await
            .unwrap();

        assert!(result.success, "got: {}", result.output);
        let msg = rx.recv().await.unwrap();
        assert_eq!(msg.media.len(), 1);
        let attached = std::path::PathBuf::from(&msg.media[0]);
        let canonical_attached = std::fs::canonicalize(&attached).unwrap_or(attached);
        let canonical_intended =
            std::fs::canonicalize(&intended).unwrap_or_else(|_| intended.clone());
        assert_eq!(
            canonical_attached, canonical_intended,
            "relative send_file MUST attach the workspace copy, not the upload-tmpdir collision",
        );

        // Cleanup: remove the planted file under the global upload root.
        let _ = std::fs::remove_file(&upload_clone);
        let _ = std::fs::remove_file(upload_root.join(upload_name));
    }

    #[tokio::test]
    async fn rejects_stale_slides_backup_artifacts() {
        let base = tempfile::tempdir().unwrap();
        let backup_dir = base.path().join("slides/demo/output_old");
        std::fs::create_dir_all(&backup_dir).unwrap();
        let backup = backup_dir.join("deck.pptx");
        std::fs::write(&backup, "pptx data").unwrap();

        let (tx, mut rx) = mpsc::channel(16);
        let tool = SendFileTool::with_context(tx, "telegram", "12345").with_base_dir(base.path());

        let result = tool
            .execute(&serde_json::json!({
                "file_path": "slides/demo/output_old/deck.pptx"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.output.contains("stale slides backup artifact"));
        assert!(rx.try_recv().is_err());
    }
}
