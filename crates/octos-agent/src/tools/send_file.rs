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
    /// Deliver only from `base_dir`, pinned to where it resolved when set:
    /// neither the managed upload root (shared by a profile's sessions) nor
    /// `extra_allowed_dirs`, and nothing at all once `base_dir` resolves
    /// elsewhere, e.g. a chat's folder replaced by a symlink
    /// (UPCR-2026-R1EFC56A3).
    base_dir_only: Option<PathBuf>,
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
            base_dir_only: None,
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
            base_dir_only: None,
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

    /// Deliver files from `base_dir` only (a chat's own workspace), and only
    /// while it resolves to `root`, where it resolved when its session was
    /// admitted. Call after [`Self::with_base_dir`].
    pub fn with_base_dir_only(mut self, root: PathBuf) -> Self {
        self.base_dir_only = Some(root);
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
            if self
                .base_dir_only
                .as_ref()
                .is_some_and(|pinned| *pinned != canonical_base)
            {
                return Ok(ToolResult {
                    output: "Error: the workspace folder is no longer available".into(),
                    success: false,
                    ..Default::default()
                });
            }
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
                        || (self.base_dir_only.is_none()
                            && (canonical_path.starts_with(&upload_root)
                                || extra_canonical
                                    .iter()
                                    .any(|d| canonical_path.starts_with(d))));
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
