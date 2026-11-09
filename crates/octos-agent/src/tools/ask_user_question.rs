//! `ask_user_question` — structured mid-turn user question (UPCR-2026-023).
//!
//! The model-visible AskUserQuestion tool. It carries 1–4 multiple-choice
//! questions (each with a short `header`, a `question`, 2–4 labeled `options`,
//! and a `multi_select` flag); the server forces a free-text "Other" escape
//! hatch on every question.
//!
//! This is the synchronous, answer-routed superset of the codex
//! `request_user_input` tool. It mirrors the proven approval flow end-to-end:
//!
//! - When a capable client is attached for the turn (the interactive server
//!   wires a [`UserQuestionRequester`] into the [`USER_QUESTION_CTX`]
//!   task-local), `execute` builds a [`UserQuestionRequest`] and `await`s the
//!   requester — exactly the boundary `shell`'s approval gate blocks at — then
//!   returns the per-question answers to the model.
//! - When NO requester is attached, the tool does NOT block. It degrades to a
//!   `request_user_input`-style structured-metadata + generic-text result and
//!   the turn continues (the agent can fall back to its own best guess). A
//!   non-supporting client therefore never sees a hung turn (§4.4).

use async_trait::async_trait;
use eyre::{Result, eyre};
use octos_core::ui_protocol::{UserQuestion, UserQuestionOption};
use serde_json::{Value, json};

use super::{
    Tool, ToolContext, ToolResult, USER_QUESTION_CTX, UserQuestionOutcome, UserQuestionRequest,
};

/// Inclusive bounds the spec pins on a single AskUserQuestion call.
const MIN_QUESTIONS: usize = 1;
const MAX_QUESTIONS: usize = 4;
const MIN_OPTIONS: usize = 2;
const MAX_OPTIONS: usize = 4;
/// `header` is a short label; the spec caps it at 12 characters. Over-long
/// headers are TRUNCATED to this ceiling (not rejected) — see [`clamp_header`].
const MAX_HEADER_CHARS: usize = 12;
/// Ellipsis appended when a `header` is truncated. Counts toward the
/// [`MAX_HEADER_CHARS`] budget so the final label never exceeds the ceiling.
const HEADER_TRUNCATION_ELLIPSIS: char = '\u{2026}';

/// Clamp a `header` to [`MAX_HEADER_CHARS`] on a char boundary, appending an
/// ellipsis when truncation occurs. The ellipsis is counted INSIDE the budget
/// so the returned label is always `<= MAX_HEADER_CHARS` characters. A header
/// already within the limit is returned verbatim. Char-based (not byte-based)
/// so multibyte labels ("颜色偏好…") clamp correctly.
fn clamp_header(header: &str) -> String {
    if header.chars().count() <= MAX_HEADER_CHARS {
        return header.to_owned();
    }
    // Reserve one char for the ellipsis so the total stays within the ceiling.
    let keep = MAX_HEADER_CHARS.saturating_sub(1);
    let mut clamped: String = header.chars().take(keep).collect();
    clamped.push(HEADER_TRUNCATION_ELLIPSIS);
    clamped
}

/// Structured user-question tool (`ask_user_question`).
#[derive(Debug, Default)]
pub struct AskUserQuestionTool;

impl AskUserQuestionTool {
    pub fn new() -> Self {
        Self
    }
}

/// Parse + validate the LLM-supplied `questions` array against the spec
/// bounds. Returns a typed input error so the model sees the failure and can
/// retry with corrected arguments. The server forces `allow_free_text = true`
/// on every question regardless of what the model sent — the "Other" escape
/// hatch is always offered (UPCR-2026-023 "Decision").
fn parse_questions(args: &Value) -> Result<Vec<UserQuestion>> {
    let raw = args
        .get("questions")
        .ok_or_else(|| eyre!("ask_user_question: missing required `questions` array"))?
        .as_array()
        .ok_or_else(|| eyre!("ask_user_question: `questions` must be an array"))?;

    if raw.len() < MIN_QUESTIONS || raw.len() > MAX_QUESTIONS {
        return Err(eyre!(
            "ask_user_question: expected {MIN_QUESTIONS}..={MAX_QUESTIONS} questions, got {}",
            raw.len()
        ));
    }

    let mut questions = Vec::with_capacity(raw.len());
    for (idx, entry) in raw.iter().enumerate() {
        let entry = entry
            .as_object()
            .ok_or_else(|| eyre!("ask_user_question: question {idx} must be an object"))?;

        let header_raw = entry
            .get("header")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .ok_or_else(|| {
                eyre!("ask_user_question: question {idx} requires a non-empty `header`")
            })?;
        // Header length is a COSMETIC ceiling (the picker renders a short tab
        // label). Real LLMs routinely send a longer descriptive header
        // ("Favorite Color") — hard-failing there made the whole call fail and
        // the agent degrade (live mini5 soak: a DeepSeek call with a 14-char
        // header hard-erred, then the retry fell to the text fallback). Clamp
        // it char-boundary-safe instead of rejecting so the request stays
        // answerable. The truncation marker keeps the label readable when it
        // overflows.
        let header = clamp_header(header_raw);

        let question = entry
            .get("question")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .ok_or_else(|| {
                eyre!("ask_user_question: question {idx} requires a non-empty `question`")
            })?
            .to_owned();

        let raw_options = entry
            .get("options")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                eyre!("ask_user_question: question {idx} requires an `options` array")
            })?;
        if raw_options.len() < MIN_OPTIONS || raw_options.len() > MAX_OPTIONS {
            return Err(eyre!(
                "ask_user_question: question {idx} expected {MIN_OPTIONS}..={MAX_OPTIONS} options, got {}",
                raw_options.len()
            ));
        }

        let mut options = Vec::with_capacity(raw_options.len());
        for (opt_idx, opt) in raw_options.iter().enumerate() {
            let opt = opt.as_object().ok_or_else(|| {
                eyre!("ask_user_question: question {idx} option {opt_idx} must be an object")
            })?;
            let label = opt
                .get("label")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .ok_or_else(|| {
                    eyre!(
                        "ask_user_question: question {idx} option {opt_idx} requires a non-empty `label`"
                    )
                })?
                .to_owned();
            // `description` is optional-friendly: default to empty so a terse
            // model call still validates.
            let description = opt
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            options.push(UserQuestionOption { label, description });
        }

        let multi_select = entry
            .get("multi_select")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        questions.push(UserQuestion {
            header,
            question,
            options,
            multi_select,
            // Server-forced: a free-text "Other" is always offered.
            allow_free_text: true,
        });
    }

    Ok(questions)
}

/// Generic fallback `title`/`body` text derived from the validated questions.
/// These are mandatory on the wire so a client that does not understand the
/// structured `questions` field still renders something actionable.
fn fallback_title_body(questions: &[UserQuestion]) -> (String, String) {
    let title = if questions.len() == 1 {
        questions[0].question.clone()
    } else {
        format!("The agent has {} questions for you", questions.len())
    };
    let body = questions
        .iter()
        .enumerate()
        .map(|(idx, q)| {
            let opts = q
                .options
                .iter()
                .map(|o| o.label.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "{}. {} (options: {opts}; or reply with your own)",
                idx + 1,
                q.question
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    (title, body)
}

/// Build the graceful-fallback result emitted when no host response channel is
/// attached. Mirrors `request_user_input_body` in `coding_tools.rs`: the model
/// sees that no synchronous answer is available and continues the turn.
fn unsupported_fallback_result(args: &Value, questions: &[UserQuestion]) -> ToolResult {
    let (title, body) = fallback_title_body(questions);
    ToolResult {
        output: json!({
            "ok": true,
            "kind": "user_question_request",
            "status": "requested",
            "title": title,
            "body": body,
            "request": args,
            "answers": null,
            "message": "User question recorded in the transcript; no synchronous host response channel is attached to this runtime (non-interactive or unattended run). Do NOT wait or re-ask: pick the option you would recommend, state that assumption in one line, and continue the task so the user can redirect you later if needed."
        })
        .to_string(),
        success: true,
        structured_metadata: Some(json!({
            "codex_tool": "ask_user_question",
            "request": args,
            "host_response_channel": "not_attached",
        })),
        ..Default::default()
    }
}

#[async_trait]
impl Tool for AskUserQuestionTool {
    fn name(&self) -> &str {
        "ask_user_question"
    }

    fn description(&self) -> &str {
        "Ask the user a structured multiple-choice question mid-turn and block on \
         the answer. Carry 1-4 questions, each with a short `header` (kept to <=12 \
         chars — a longer header is truncated, not rejected), a `question`, 2-4 \
         `options` (each `label` + `description`), and a `multi_select` flag. The \
         user is always also offered a free-text \"Other\". Use this instead of \
         guessing when a bounded choice would resolve an ambiguity (which \
         framework, which file, opt-in to a cleanup)."
    }

    fn tags(&self) -> &[&str] {
        &["code"]
    }

    /// This tool BLOCKS on the human until the client answers via
    /// `user_question/respond` (the `request_user_question` await below),
    /// exactly as the approval gate blocks on the approval requester. It must
    /// therefore be exempt from the dispatch-boundary timeout: a human may
    /// take longer than any finite ceiling, and firing the Gap-3.3 timeout
    /// would drop the requester's receiver and leak the pending question
    /// store entry forever. The turn-interrupt drain is the correct
    /// cancellation path (resolves the waiter as `Cancelled`). See
    /// `ToolRegistry::execute_with_context` and the `LONG_RUNNING_TOOLS`
    /// batch-level exemption in `agent::execution`.
    fn blocks_on_human_input(&self) -> bool {
        true
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "minItems": MIN_QUESTIONS,
                    "maxItems": MAX_QUESTIONS,
                    "description": "1-4 multiple-choice questions to ask the user.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "header": {
                                "type": "string",
                                "maxLength": MAX_HEADER_CHARS,
                                "description": "Short label for the question (kept to <=12 chars; a longer header is truncated server-side, not rejected)."
                            },
                            "question": {
                                "type": "string",
                                "description": "The question text."
                            },
                            "options": {
                                "type": "array",
                                "minItems": MIN_OPTIONS,
                                "maxItems": MAX_OPTIONS,
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": { "type": "string" },
                                        "description": { "type": "string" }
                                    },
                                    "required": ["label"]
                                }
                            },
                            "multi_select": {
                                "type": "boolean",
                                "description": "When true the user may select more than one option."
                            }
                        },
                        "required": ["header", "question", "options"]
                    }
                }
            },
            "required": ["questions"]
        })
    }

    async fn execute(&self, args: &Value) -> Result<ToolResult> {
        self.execute_with_context(&ToolContext::zero(), args).await
    }

    async fn execute_with_context(&self, _ctx: &ToolContext, args: &Value) -> Result<ToolResult> {
        // Validate first so a malformed call fails the same way regardless of
        // whether a client is attached.
        let questions = parse_questions(args)?;

        // No interactive client supporting user_question.v1 → graceful
        // fallback (§4.4): do not block, emit structured metadata, continue.
        let Some(requester) = USER_QUESTION_CTX.try_with(Clone::clone).ok() else {
            tracing::debug!(
                target: "octos::tools::ask_user_question",
                question_count = questions.len(),
                "no user-question requester attached; degrading to structured-metadata fallback"
            );
            return Ok(unsupported_fallback_result(args, &questions));
        };

        let (title, body) = fallback_title_body(&questions);
        // Keep a copy of the parsed questions so the `Unsupported` fallback
        // (a requester that could not surface the prompt) can describe the
        // REAL questions even after `request` is moved into the requester (#6).
        let questions_for_fallback = questions.clone();
        let request = UserQuestionRequest {
            questions,
            title,
            body,
        };

        // This is the await boundary: the turn is now PAUSED until the client
        // answers via user_question/respond (or the turn is interrupted).
        match requester.request_user_question(request).await {
            UserQuestionOutcome::Answered(answers) => {
                let answers_value = serde_json::to_value(&answers).unwrap_or(Value::Null);
                Ok(ToolResult {
                    output: json!({
                        "ok": true,
                        "kind": "user_question_answer",
                        "status": "answered",
                        "answers": answers_value,
                    })
                    .to_string(),
                    success: true,
                    structured_metadata: Some(json!({
                        "codex_tool": "ask_user_question",
                        "host_response_channel": "attached",
                        "answers": serde_json::to_value(&answers).unwrap_or(Value::Null),
                    })),
                    ..Default::default()
                })
            }
            UserQuestionOutcome::Cancelled => Ok(ToolResult {
                output: json!({
                    "ok": false,
                    "kind": "user_question_answer",
                    "status": "cancelled",
                    "answers": null,
                    "message": "User question was cancelled before an answer arrived (turn interrupted)."
                })
                .to_string(),
                success: false,
                ..Default::default()
            }),
            // A requester was attached but reported it could not surface the
            // question (e.g. wire delivery failed). Degrade like the no-context
            // path so the turn never hard-blocks — and describe the ACTUAL
            // parsed questions (carried on `request.questions`) so the
            // structured-metadata/text fallback reflects the real prompt
            // rather than an empty one (#6).
            UserQuestionOutcome::Unsupported => {
                Ok(unsupported_fallback_result(args, &questions_for_fallback))
            }
        }
    }
}
