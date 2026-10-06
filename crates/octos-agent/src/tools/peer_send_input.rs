//! `peer_send_input` — master→peer cross-session input injection (#436).
//!
//! When the master's LLM needs to give a follow-up instruction to a RUNNING
//! peer session, this tool hands the message to a host callback (wired during
//! turn construction in the serve/WS path). The tool carries no IPC knowledge;
//! the callback picks the delivery path: the gateway in-process actor inbox
//! when present, else the serve master-continuation queue (which delivers the
//! injection as the peer's next turn on its own connection).
//!
//! Guard rails:
//! - Depth-1: the tool is never registered on peer sessions themselves (same
//!   guard as `peer_handoff`).
//! - Authorization: only the peer's recorded originator may inject (enforced
//!   host-side by the callback).
//! - Max message size: PEER_SEND_INPUT_MAX_BYTES (64 KB).
//! - Each call carries a unique occurrence id so two distinct sends never
//!   collapse while a true retry dedups.
//! - Returns a clear error when the peer session is not open / not authorized /
//!   could not be durably queued.
//!
//! Security model (single-user-per-profile / Option C): authorization is
//! session-scoped WITHIN one user's own trust domain — in serve, profile ===
//! user id, so a profile is a single user. Cross-USER injection is blocked by
//! profile scoping; an LLM cannot cross-session-inject (it can't `session/open`
//! and the caller session is server-captured), so the originator check blocks
//! the meaningful in-band threat. The residual same-user cross-session case (a
//! client deliberately opening the owner session) is the user's own authority
//! within their own profile, by design. See the host-side
//! `peer_send_input_authorized` for the full rationale.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use super::{Tool, ToolResult};

/// Hard cap on the message payload (bytes).
pub const PEER_SEND_INPUT_MAX_BYTES: usize = 64 * 1024;

/// Facts the host callback needs to locate the peer session.
#[derive(Debug, Clone)]
pub struct PeerSendInputRequest {
    /// The peer IDENTIFIER — its display name or slug (as reported by
    /// peer_handoff / peer_list). The host resolves it to the directory slug.
    pub slug: String,
    /// The message to inject as a new turn into the peer session.
    pub message: String,
    /// #436 P1 — a UNIQUE-per-tool-call occurrence id (the LLM `tool_call_id`
    /// when available, else a process-unique fallback). The continuation-queue
    /// producer embeds this in its dedupe key so two DISTINCT injections
    /// (separate tool calls, even with identical text) do NOT collapse, while a
    /// genuine retry of the SAME call (same id) still dedups — the invariant
    /// the scheduler requires of every `External` producer.
    pub occurrence_id: String,
}

/// Monotonic fallback for the occurrence id when no `tool_call_id` is present
/// (`ToolContext::zero()` — tests / non-context callers). Process-unique, which
/// is all the in-memory dedupe needs.
static PEER_SEND_INPUT_OCCURRENCE_SEQ: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// What the host did with a `peer_send_input` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerSendInputDelivery {
    /// Newly queued: the peer will process it as its next turn.
    Queued,
    /// This same tool call (same occurrence id) was already queued, so the
    /// retry was not queued again. Not a new message to the peer.
    AlreadyQueued,
}

/// Host callback that delivers a message to a running peer session's inbox.
pub type PeerSendInputCallback =
    Arc<dyn Fn(PeerSendInputRequest) -> Result<PeerSendInputDelivery, String> + Send + Sync>;

/// A host's refusal of a delivered input (UPCR-2026-035 `peer/input/reject`),
/// as the model reads it: `peer_input_rejected: <reason>` and, when the host
/// gave one, its short message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSendInputRefusal(pub String);

/// Host callback that waits, briefly, for the answer to an input it just
/// delivered: `Some(refusal)` when the receiving host refused it, `None` when
/// the peer started on it, the wait ran out, or the peer has no such answer.
pub type PeerSendInputAnswerCallback = Arc<
    dyn Fn(PeerSendInputRequest) -> BoxFuture<'static, Option<PeerSendInputRefusal>> + Send + Sync,
>;

/// `peer_send_input` tool. See the module docs for the cross-session channel.
pub struct PeerSendInputTool {
    send_input: PeerSendInputCallback,
    await_answer: Option<PeerSendInputAnswerCallback>,
}

impl PeerSendInputTool {
    pub fn new(send_input: PeerSendInputCallback) -> Self {
        Self {
            send_input,
            await_answer: None,
        }
    }

    /// After a delivery, wait for the receiving host's answer so a refusal
    /// fails this call with its reason.
    pub fn with_answer(mut self, await_answer: PeerSendInputAnswerCallback) -> Self {
        self.await_answer = Some(await_answer);
        self
    }
}

#[derive(Debug, Deserialize)]
struct Input {
    slug: String,
    message: String,
}

#[async_trait]
impl Tool for PeerSendInputTool {
    fn name(&self) -> &str {
        "peer_send_input"
    }

    fn description(&self) -> &str {
        "Send a follow-up message to a RUNNING peer identified by its NAME (or \
         slug), as reported by peer_handoff / peer_list. The peer receives it as \
         its next turn. Use when a deployed peer needs steering, additional \
         context, or a correction — but only when the peer was staged earlier in \
         THIS conversation or the user confirms the name. The peer MUST be running \
         (the user opened the staged session); if it has completed or is idle \
         this will fail with an error."
    }

    fn tags(&self) -> &[&str] {
        &["gateway"]
    }

    fn concurrency_class(&self) -> super::ConcurrencyClass {
        super::ConcurrencyClass::Safe
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["slug", "message"],
            "properties": {
                "slug": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Peer NAME (or slug) to send to, as reported by peer_handoff or peer_list."
                },
                "message": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": PEER_SEND_INPUT_MAX_BYTES,
                    "description": "The message to inject as the peer's next turn. Include all needed context — the peer cannot see this conversation."
                }
            }
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        // Migration pattern: the real logic lives in `execute_with_context` so
        // it can read the `tool_call_id`; the context-free entry delegates with
        // a zero context (which yields the process-unique fallback occurrence).
        self.execute_with_context(&super::ToolContext::zero(), args)
            .await
    }

    async fn execute_with_context(
        &self,
        ctx: &super::ToolContext,
        args: &Value,
    ) -> Result<ToolResult> {
        let input: Input = match serde_json::from_value(args.clone()) {
            Ok(i) => i,
            Err(e) => {
                return Ok(ToolResult {
                    output: format!(
                        "invalid peer_send_input arguments: {e}. \
                         Required: {{\"slug\": string, \"message\": string}}"
                    ),
                    success: false,
                    ..Default::default()
                });
            }
        };

        let slug = input.slug.trim();
        let message = input.message.trim();

        if slug.is_empty() {
            return Ok(ToolResult {
                output: "peer_send_input requires a non-empty slug".to_string(),
                success: false,
                ..Default::default()
            });
        }

        if message.is_empty() {
            return Ok(ToolResult {
                output: "peer_send_input requires a non-empty message".to_string(),
                success: false,
                ..Default::default()
            });
        }

        if message.len() > PEER_SEND_INPUT_MAX_BYTES {
            return Ok(ToolResult {
                output: format!(
                    "message exceeds {PEER_SEND_INPUT_MAX_BYTES} bytes — \
                     keep the directive concise, include context in the workspace"
                ),
                success: false,
                ..Default::default()
            });
        }

        // #436 P1 (#4) — the occurrence id makes THIS tool call unique so two
        // distinct sends don't collapse. Prefer the LLM `tool_call_id` (stable
        // across a framework retry of the same call → a true retry dedups);
        // fall back to a process-unique counter when absent.
        let occurrence_id = if ctx.tool_id.trim().is_empty() {
            format!(
                "seq-{}",
                PEER_SEND_INPUT_OCCURRENCE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )
        } else {
            ctx.tool_id.clone()
        };

        let request = PeerSendInputRequest {
            slug: slug.to_string(),
            message: message.to_string(),
            occurrence_id,
        };

        let delivery = (self.send_input)(request.clone());
        if delivery.is_ok() {
            if let Some(await_answer) = &self.await_answer {
                if let Some(PeerSendInputRefusal(refusal)) = await_answer(request).await {
                    return Ok(ToolResult {
                        output: format!(
                            "{refusal} — the app that owns peer {slug} refused this input, \
                             so the peer did not act on it"
                        ),
                        success: false,
                        ..Default::default()
                    });
                }
            }
        }
        match delivery {
            Ok(PeerSendInputDelivery::Queued) => Ok(ToolResult {
                output: format!(
                    "message sent to peer {slug} — \
                     the peer will process it as its next turn"
                ),
                success: true,
                ..Default::default()
            }),
            // A retry of THIS call collapsed onto the copy it already queued.
            // Say so rather than claim a fresh send.
            Ok(PeerSendInputDelivery::AlreadyQueued) => Ok(ToolResult {
                output: format!(
                    "already queued by this call — peer {slug} has this input \
                     (not sent again)"
                ),
                success: true,
                ..Default::default()
            }),
            Err(e) => Ok(ToolResult {
                output: format!("failed to send to peer {slug}: {e}"),
                success: false,
                ..Default::default()
            }),
        }
    }
}
