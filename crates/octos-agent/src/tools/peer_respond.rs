//! `peer_respond` — master→peer answer for a BLOCKED peer (human-in-the-loop).
//!
//! A peer session can PARK mid-turn on an interactive prompt — a tool-approval
//! request or a clarifying `ask_user_question` — exactly like any session
//! does, awaiting a oneshot that today only a client `approval/respond` /
//! `user_question/respond` RPC resolves. When the peer is autonomous there is
//! no human at its UI, so the MASTER that staged it becomes the
//! human-in-the-loop: `peer_list` surfaces the peer as `awaiting_input` (read
//! from the process-global pending approval/question stores — the single
//! authority, so a parked peer stays visible while it remains open), and THIS
//! tool answers a specific one. (A CLOSED peer never parks at all — see
//! CLOSE-WHILE-PARKED below.)
//!
//! The tool carries no IPC knowledge; a host callback (wired during turn
//! construction in the serve/WS path, right beside `peer_send_input`) locates
//! the peer's pending approval/question by its id and RESOLVES the very oneshot
//! the client RPC would — through the SAME process-global pending store — using
//! the peer's TRUSTED session key derived from the wire registry.
//!
//! Guard rails (IDENTICAL to `peer_send_input`):
//! - Depth-1: never registered on peer sessions themselves.
//! - Authorization: only the peer's recorded originator may respond.
//! - A clear error when the peer is not open / not awaiting input / not
//!   authorized / already resolved, when the response KIND does not match, or
//!   when the peer has MULTIPLE pending prompts and no `id` was supplied.
//!
//! CLOSE-WHILE-PARKED (#1842, fixed). Closing a peer cancels its
//! CURRENTLY-parked prompt fail-closed. That alone left a race: `peer_close`
//! did not abort the peer's in-flight turn, so a peer that PARKED AGAIN after
//! the close sweep — or reopened under a fresh wire session for the same
//! `(profile, slug)` — could register a new pending prompt that `peer_list`
//! hides (closed) and `peer_respond` refuses (closed), leaving it parked until
//! the connection tore down. Two host-side mechanisms close it:
//!
//! - **The peer STOPS on close.** `peer_close` runs the same interrupt routine
//!   `turn/interrupt` uses against the peer's active turn (`Active` →
//!   `Interrupting` + the per-turn `interrupt_tx` signal). That is the path the
//!   turn loop honors, so it aborts the peer's INNER agent task rather than
//!   detaching it — the peer cannot reach another park point.
//! - **A closed peer's parks are REFUSED.** Both park points (tool approval and
//!   `ask_user_question`) gate on the peer's `(profile, slug)` closed state,
//!   resolved from the turn's RESOLVED runtime profile (so raw client sessions,
//!   whose session key carries no profile, are covered too) and the peer's
//!   validated topic, read from the durable `closed` marker. The gate brackets
//!   the registration — precheck, register, post-check — so a close landing
//!   inside the check-then-park window is caught by either the post-check or
//!   the close's own pending sweep. Staging a peer mints a fresh
//!   `peers/<slug>`, so a restaged peer parks normally again.

use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;

use super::{Tool, ToolResult};

/// Hard cap on a single free-text answer payload (bytes) — an answer is a short
/// human reply, not a document.
pub const PEER_RESPOND_ANSWER_MAX_BYTES: usize = 8 * 1024;

/// One structured answer to ONE of the peer's pending questions — mirrors the
/// server's `UserQuestionAnswer` so a multi-question prompt (1–4 questions) can
/// be answered fully. A bare `answer` string becomes a single free-text entry.
#[derive(Debug, Clone)]
pub struct PeerRespondAnswer {
    /// Chosen option label(s) for this question (empty for a free-text reply).
    pub selected_labels: Vec<String>,
    /// Free-text reply for this question (the "Other" path).
    pub free_text: Option<String>,
}

/// Facts the host callback needs to locate the peer's pending prompt and
/// resolve it. Exactly one of `decision` / `answers` is set — validated by the
/// tool before the callback runs, and re-checked host-side against the peer's
/// actual pending KIND.
#[derive(Debug, Clone)]
pub struct PeerRespondRequest {
    /// The peer IDENTIFIER — its display name or slug (as reported by
    /// `peer_list`). The host resolves it to the directory slug.
    pub slug: String,
    /// The specific pending prompt id to answer (as listed by `peer_list`).
    /// Optional when the peer has exactly ONE pending prompt; REQUIRED when it
    /// has more than one (the host returns a clear error otherwise).
    pub id: Option<String>,
    /// For a pending APPROVAL: `"approve"` or `"deny"`. `None` for a question.
    pub decision: Option<String>,
    /// For a pending QUESTION: one answer per question (in question order). A
    /// single-question prompt takes one entry; the ergonomic `answer` string
    /// becomes `[{free_text}]`. `None` for an approval.
    pub answers: Option<Vec<PeerRespondAnswer>>,
}

/// Host callback that resolves a peer session's parked approval/question.
pub type PeerRespondCallback = Arc<dyn Fn(PeerRespondRequest) -> Result<(), String> + Send + Sync>;

/// `peer_respond` tool. See the module docs for the cross-session bridge.
pub struct PeerRespondTool {
    respond: PeerRespondCallback,
}

impl PeerRespondTool {
    pub fn new(respond: PeerRespondCallback) -> Self {
        Self { respond }
    }
}

/// One element of the `answers` array — either a bare string (free text / an
/// option label) or a structured `{labels, text}` object for full fidelity
/// (multi-select, or an explicit option choice).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum AnswerArg {
    Text(String),
    Structured {
        #[serde(default)]
        labels: Vec<String>,
        #[serde(default)]
        text: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
struct Input {
    slug: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    decision: Option<String>,
    #[serde(default)]
    answer: Option<String>,
    #[serde(default)]
    answers: Option<Vec<AnswerArg>>,
}

fn fail(output: impl Into<String>) -> ToolResult {
    ToolResult {
        output: output.into(),
        success: false,
        ..Default::default()
    }
}

#[async_trait]
impl Tool for PeerRespondTool {
    fn name(&self) -> &str {
        "peer_respond"
    }

    fn description(&self) -> &str {
        "Answer a peer that is BLOCKED waiting for input — you act as its \
         human-in-the-loop. peer_list shows such a peer as `awaiting_input` with \
         each pending prompt's id + summary. Identify the peer by NAME (or slug); \
         when it has more than one pending prompt, pass the specific `id`. For a \
         pending tool-APPROVAL pass decision=\"approve\"/\"deny\"; for a pending \
         QUESTION pass answer=\"<reply>\" (a single-question prompt), or answers=\
         [ ... ] with one entry per question (each a string, or {labels:[...], \
         text:\"...\"}). Only the peer you staged, while it is actually awaiting \
         input, can be answered. Provide EXACTLY ONE of decision or answer/answers."
    }

    fn tags(&self) -> &[&str] {
        // Same visibility surface as peer_send_input / peer_close — the
        // master-control peer tools share one tag filter.
        &["gateway"]
    }

    fn concurrency_class(&self) -> super::ConcurrencyClass {
        // Resolves a specific pending approval/question oneshot in the shared
        // process-global store; serialize so two concurrent peer_respond calls
        // can't race on the same pending set.
        super::ConcurrencyClass::Exclusive
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["slug"],
            "properties": {
                "slug": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Peer NAME (or slug) to answer, as reported by peer_list."
                },
                "id": {
                    "type": "string",
                    "description": "The specific pending prompt id (from peer_list). Optional when the peer has exactly one pending prompt; required when it has several."
                },
                "decision": {
                    "type": "string",
                    "enum": ["approve", "deny"],
                    "description": "For a pending tool-APPROVAL: approve or deny it. Omit for a question."
                },
                "answer": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": PEER_RESPOND_ANSWER_MAX_BYTES,
                    "description": "For a single-question prompt: your reply (may match an offered option). Omit for an approval or when using `answers`."
                },
                "answers": {
                    "type": "array",
                    "description": "For a multi-question prompt: one entry per question, each a string (free text / option label) or an object {labels:[string], text:string}.",
                    "items": {
                        "oneOf": [
                            { "type": "string" },
                            {
                                "type": "object",
                                "properties": {
                                    "labels": { "type": "array", "items": { "type": "string" } },
                                    "text": { "type": "string" }
                                }
                            }
                        ]
                    }
                }
            }
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        let input: Input = match serde_json::from_value(args.clone()) {
            Ok(i) => i,
            Err(e) => {
                return Ok(fail(format!(
                    "invalid peer_respond arguments: {e}. Required: {{\"slug\": string}} plus \
                     EXACTLY ONE of {{\"decision\": \"approve\"|\"deny\"}} or \
                     {{\"answer\": string}} / {{\"answers\": [...]}}"
                )));
            }
        };

        let slug = input.slug.trim();
        if slug.is_empty() {
            return Ok(fail("peer_respond requires a non-empty slug"));
        }

        let id = input
            .id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);

        // Normalize: an empty-string decision counts as absent.
        let decision = input
            .decision
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);

        // Build the structured answers: explicit `answers` wins; otherwise the
        // ergonomic single `answer` string becomes one free-text entry.
        let answers: Option<Vec<PeerRespondAnswer>> = match input.answers {
            Some(list) => {
                let mut out = Vec::with_capacity(list.len());
                for item in list {
                    let ans = match item {
                        AnswerArg::Text(text) => {
                            let text = text.trim();
                            if text.is_empty() {
                                return Ok(fail("peer_respond answers entries must be non-empty"));
                            }
                            if text.len() > PEER_RESPOND_ANSWER_MAX_BYTES {
                                return Ok(fail(format!(
                                    "an answer exceeds {PEER_RESPOND_ANSWER_MAX_BYTES} bytes — keep it concise"
                                )));
                            }
                            PeerRespondAnswer {
                                selected_labels: Vec::new(),
                                free_text: Some(text.to_owned()),
                            }
                        }
                        AnswerArg::Structured { labels, text } => {
                            let labels: Vec<String> = labels
                                .into_iter()
                                .map(|l| l.trim().to_owned())
                                .filter(|l| !l.is_empty())
                                .collect();
                            let text = text
                                .as_deref()
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(str::to_owned);
                            if labels.is_empty() && text.is_none() {
                                return Ok(fail("each answers entry needs a label or free text"));
                            }
                            PeerRespondAnswer {
                                selected_labels: labels,
                                free_text: text,
                            }
                        }
                    };
                    out.push(ans);
                }
                if out.is_empty() {
                    return Ok(fail("peer_respond answers must not be empty"));
                }
                Some(out)
            }
            None => input
                .answer
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|answer| {
                    vec![PeerRespondAnswer {
                        selected_labels: Vec::new(),
                        free_text: Some(answer.to_owned()),
                    }]
                }),
        };

        // Exactly one of decision / answers.
        match (&decision, &answers) {
            (Some(_), Some(_)) => {
                return Ok(fail(
                    "peer_respond takes EXACTLY ONE of decision or answer/answers, not both — \
                     use decision for a pending approval, answer(s) for a pending question",
                ));
            }
            (None, None) => {
                return Ok(fail(
                    "peer_respond needs one of decision (\"approve\"/\"deny\", for a pending \
                     approval) or answer/answers (for a pending question)",
                ));
            }
            _ => {}
        }

        if let Some(d) = &decision {
            if d != "approve" && d != "deny" {
                return Ok(fail(format!(
                    "invalid decision '{d}' — use \"approve\" or \"deny\""
                )));
            }
        }

        let request = PeerRespondRequest {
            slug: slug.to_string(),
            id,
            decision: decision.clone(),
            answers,
        };

        match (self.respond)(request) {
            Ok(()) => {
                let what = match &decision {
                    Some(d) => format!("approval {d}d"),
                    None => "answer delivered".to_string(),
                };
                Ok(ToolResult {
                    output: format!("peer {slug} unblocked — {what}; it will resume its turn"),
                    success: true,
                    ..Default::default()
                })
            }
            Err(e) => Ok(fail(format!("failed to respond to peer {slug}: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    fn tool_capturing(sink: Arc<Mutex<Vec<PeerRespondRequest>>>) -> PeerRespondTool {
        PeerRespondTool::new(Arc::new(move |req: PeerRespondRequest| {
            sink.lock().unwrap().push(req);
            Ok(())
        }))
    }

    #[tokio::test]
    async fn should_forward_approve_decision_to_callback() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let tool = tool_capturing(sink.clone());
        let result = tool
            .execute(&json!({ "slug": "ci-fix", "decision": "approve" }))
            .await
            .unwrap();
        assert!(result.success, "unexpected failure: {}", result.output);
        let calls = sink.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].slug, "ci-fix");
        assert_eq!(calls[0].decision.as_deref(), Some("approve"));
        assert!(calls[0].answers.is_none());
    }

    #[tokio::test]
    async fn should_forward_single_answer_as_one_free_text_entry() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let tool = tool_capturing(sink.clone());
        let result = tool
            .execute(&json!({ "slug": "alpha", "answer": "use postgres" }))
            .await
            .unwrap();
        assert!(result.success, "unexpected failure: {}", result.output);
        let calls = sink.lock().unwrap();
        let answers = calls[0].answers.as_ref().unwrap();
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].free_text.as_deref(), Some("use postgres"));
        assert!(answers[0].selected_labels.is_empty());
        assert!(calls[0].decision.is_none());
    }

    #[tokio::test]
    async fn should_forward_multi_answers_array_string_and_structured() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let tool = tool_capturing(sink.clone());
        let result = tool
            .execute(&json!({
                "slug": "alpha",
                "answers": ["postgres", { "labels": ["Yes"], "text": "with SSL" }]
            }))
            .await
            .unwrap();
        assert!(result.success, "unexpected failure: {}", result.output);
        let calls = sink.lock().unwrap();
        let answers = calls[0].answers.as_ref().unwrap();
        assert_eq!(answers.len(), 2);
        assert_eq!(answers[0].free_text.as_deref(), Some("postgres"));
        assert_eq!(answers[1].selected_labels, vec!["Yes".to_string()]);
        assert_eq!(answers[1].free_text.as_deref(), Some("with SSL"));
    }

    #[tokio::test]
    async fn should_forward_target_id() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let tool = tool_capturing(sink.clone());
        let result = tool
            .execute(&json!({ "slug": "alpha", "id": "abc-123", "decision": "deny" }))
            .await
            .unwrap();
        assert!(result.success, "unexpected failure: {}", result.output);
        assert_eq!(sink.lock().unwrap()[0].id.as_deref(), Some("abc-123"));
    }

    #[tokio::test]
    async fn should_reject_both_decision_and_answer() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let tool = tool_capturing(sink.clone());
        let result = tool
            .execute(&json!({ "slug": "x", "decision": "deny", "answer": "no" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("EXACTLY ONE"));
        assert!(sink.lock().unwrap().is_empty(), "callback must not run");
    }

    #[tokio::test]
    async fn should_reject_neither_decision_nor_answer() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let tool = tool_capturing(sink.clone());
        let result = tool.execute(&json!({ "slug": "x" })).await.unwrap();
        assert!(!result.success);
        assert!(sink.lock().unwrap().is_empty(), "callback must not run");
    }

    #[tokio::test]
    async fn should_reject_invalid_decision_value() {
        let sink = Arc::new(Mutex::new(Vec::new()));
        let tool = tool_capturing(sink.clone());
        let result = tool
            .execute(&json!({ "slug": "x", "decision": "maybe" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("invalid decision"));
        assert!(sink.lock().unwrap().is_empty(), "callback must not run");
    }

    #[tokio::test]
    async fn should_surface_callback_error_as_tool_failure() {
        let tool = PeerRespondTool::new(Arc::new(|_req| {
            Err("peer 'x' is not awaiting input".to_string())
        }));
        let result = tool
            .execute(&json!({ "slug": "x", "decision": "approve" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.output.contains("not awaiting input"));
    }

    #[tokio::test]
    async fn should_declare_exclusive_concurrency() {
        let tool = PeerRespondTool::new(Arc::new(|_req| Ok(())));
        assert_eq!(
            tool.concurrency_class(),
            super::super::ConcurrencyClass::Exclusive,
            "peer_respond resolves shared-store oneshots and must serialize"
        );
    }
}
