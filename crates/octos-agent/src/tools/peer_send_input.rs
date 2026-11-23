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
use octos_core::task_inputs::DeclaredInput;
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
    /// Files the peer needs for THIS message's task (see
    /// [`octos_core::task_inputs`]). The host checks them against the peer's
    /// working directory and puts them into the first model request of the
    /// turn the message starts. Empty when the call declares none.
    pub inputs: Vec<DeclaredInput>,
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
    assignment: bool,
    await_answer: Option<PeerSendInputAnswerCallback>,
}

impl PeerSendInputTool {
    pub fn new(send_input: PeerSendInputCallback) -> Self {
        Self {
            send_input,
            assignment: false,
            await_answer: None,
        }
    }

    /// Same delivery primitive, with coordinator authorization supplied by
    /// the host callback. No new agent lifecycle is introduced.
    pub fn as_assignment(mut self) -> Self {
        self.assignment = true;
        self
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
    #[serde(default)]
    inputs: Option<Value>,
}

#[async_trait]
impl Tool for PeerSendInputTool {
    fn name(&self) -> &str {
        if self.assignment {
            "peer_assign"
        } else {
            "peer_send_input"
        }
    }

    fn description(&self) -> &str {
        if self.assignment {
            return "Assign work to an existing workspace peer using its workspace agent_id as slug. Only the current coordinator can assign. Include the task, file ownership, acceptance criteria and context. The member keeps its own session and permissions; busy members queue the assignment. Use peer_gather for results.";
        }
        "Send a message or task to a peer returned by peer_list. For automatic \
         same-workspace peers use its workspace agent_id as slug; the target may \
         be idle or busy (busy targets process it after their current turn). \
         The coordinator distributes tasks and gathers results; every member \
         can message other members. Preserve their user tasks and permissions. \
         Staged peers still use their existing name/slug and ownership rules. \
         Do not send acknowledgment-only replies or repeat delivery while queued."
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
                },
                "inputs": octos_core::task_inputs::declared_inputs_schema()
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

        let inputs = match octos_core::task_inputs::parse_declared_inputs(input.inputs.as_ref()) {
            Ok(inputs) => inputs,
            Err(err) => {
                return Ok(ToolResult {
                    output: format!("invalid peer_send_input arguments: {err}"),
                    success: false,
                    ..Default::default()
                });
            }
        };

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

        let with_inputs = match inputs.len() {
            0 => String::new(),
            declared => format!(
                "; the {declared} declared input(s) go into that turn's first model request"
            ),
        };
        let request = PeerSendInputRequest {
            slug: slug.to_string(),
            message: message.to_string(),
            occurrence_id,
            inputs,
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
                     the peer will process it as its next turn{with_inputs}"
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[tokio::test]
    async fn should_hand_a_messages_declared_inputs_to_the_host_and_send_nothing_malformed() {
        let seen: Arc<Mutex<Vec<PeerSendInputRequest>>> = Arc::default();
        let sink = seen.clone();
        let tool = PeerSendInputTool::new(Arc::new(move |request| {
            sink.lock().unwrap().push(request);
            Ok(PeerSendInputDelivery::Queued)
        }));
        assert!(
            tool.input_schema()["properties"]["inputs"]["description"]
                .as_str()
                .unwrap()
                .contains("FIRST model request")
        );

        let sent = tool
            .execute(&json!({
                "slug": "Edison",
                "message": "Fix the two defects in the review.",
                "inputs": ["review/round-2.md", { "path": "renders/page-03.png", "required": false }],
            }))
            .await
            .unwrap();
        assert!(sent.success, "{}", sent.output);
        assert!(sent.output.contains("the 2 declared input(s) go into"));
        assert_eq!(
            seen.lock().unwrap()[0].inputs,
            [
                DeclaredInput::required("review/round-2.md"),
                DeclaredInput::optional("renders/page-03.png"),
            ]
        );

        let refused = tool
            .execute(&json!({
                "slug": "Edison",
                "message": "Fix the two defects in the review.",
                "inputs": [{ "path": "review/round-2.md", "optional": true }],
            }))
            .await
            .unwrap();
        assert!(!refused.success);
        assert!(refused.output.contains("inputs[0]"), "{}", refused.output);
        assert_eq!(seen.lock().unwrap().len(), 1, "the host was asked to send");
    }
}
