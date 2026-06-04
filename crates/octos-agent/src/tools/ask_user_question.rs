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
/// `header` is a short label; the spec caps it at 12 characters.
const MAX_HEADER_CHARS: usize = 12;

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

        let header = entry
            .get("header")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .ok_or_else(|| {
                eyre!("ask_user_question: question {idx} requires a non-empty `header`")
            })?
            .to_owned();
        if header.chars().count() > MAX_HEADER_CHARS {
            return Err(eyre!(
                "ask_user_question: question {idx} `header` exceeds {MAX_HEADER_CHARS} characters"
            ));
        }

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
            "message": "User question recorded; no synchronous host response channel is attached to this runtime."
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
         the answer. Carry 1-4 questions, each with a short `header` (<=12 chars), \
         a `question`, 2-4 `options` (each `label` + `description`), and a \
         `multi_select` flag. The user is always also offered a free-text \"Other\". \
         Use this instead of guessing when a bounded choice would resolve an \
         ambiguity (which framework, which file, opt-in to a cleanup)."
    }

    fn tags(&self) -> &[&str] {
        &["code"]
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
                                "description": "Short label for the question (<=12 chars)."
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
            // path so the turn never hard-blocks.
            UserQuestionOutcome::Unsupported => Ok(unsupported_fallback_result(args, &[])),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{UserQuestionRequest as Req, UserQuestionRequester};
    use octos_core::ui_protocol::UserQuestionAnswer;
    use std::sync::Arc;
    use std::sync::Mutex;

    fn one_valid_question() -> Value {
        json!({
            "questions": [
                {
                    "header": "Framework",
                    "question": "Which web framework should I scaffold?",
                    "options": [
                        { "label": "axum", "description": "tower-based async" },
                        { "label": "actix", "description": "actor-based" }
                    ],
                    "multi_select": false
                }
            ]
        })
    }

    /// Records the request it received and replays a canned answer.
    struct RecordingRequester {
        captured: Mutex<Option<Req>>,
        answer: Vec<UserQuestionAnswer>,
    }

    #[async_trait]
    impl UserQuestionRequester for RecordingRequester {
        async fn request_user_question(&self, request: Req) -> UserQuestionOutcome {
            *self.captured.lock().unwrap() = Some(request);
            UserQuestionOutcome::Answered(self.answer.clone())
        }
    }

    struct CancellingRequester;

    #[async_trait]
    impl UserQuestionRequester for CancellingRequester {
        async fn request_user_question(&self, _request: Req) -> UserQuestionOutcome {
            UserQuestionOutcome::Cancelled
        }
    }

    #[tokio::test]
    async fn should_emit_structured_metadata_when_no_requester() {
        let tool = AskUserQuestionTool::new();
        let args = one_valid_question();
        let result = tool.execute(&args).await.expect("fallback ok");
        assert!(result.success);
        let meta = result
            .structured_metadata
            .as_ref()
            .expect("fallback must emit structured_metadata");
        assert_eq!(meta["codex_tool"], json!("ask_user_question"));
        assert_eq!(meta["host_response_channel"], json!("not_attached"));
        // Request round-trips into the structured event for the client.
        assert_eq!(meta["request"], args);
    }

    #[tokio::test]
    async fn should_block_and_return_answers_when_requester_present() {
        let answer = vec![UserQuestionAnswer {
            selected_labels: vec!["axum".into()],
            free_text: None,
        }];
        let requester = Arc::new(RecordingRequester {
            captured: Mutex::new(None),
            answer: answer.clone(),
        });
        let requester_dyn: Arc<dyn UserQuestionRequester> = requester.clone();

        let tool = AskUserQuestionTool::new();
        let args = one_valid_question();
        let result = USER_QUESTION_CTX
            .scope(requester_dyn, async move { tool.execute(&args).await })
            .await
            .expect("answered ok");

        assert!(result.success);
        let output: Value = serde_json::from_str(&result.output).expect("output json");
        assert_eq!(output["status"], json!("answered"));
        assert_eq!(output["answers"][0]["selected_labels"][0], json!("axum"));

        // The requester saw the validated, server-forced request.
        let captured = requester
            .captured
            .lock()
            .unwrap()
            .clone()
            .expect("captured");
        assert_eq!(captured.questions.len(), 1);
        assert!(
            captured.questions[0].allow_free_text,
            "server must force allow_free_text=true"
        );
        assert!(!captured.title.is_empty(), "mandatory generic title");
        assert!(!captured.body.is_empty(), "mandatory generic body");
    }

    #[tokio::test]
    async fn should_return_cancelled_when_turn_interrupted() {
        let requester_dyn: Arc<dyn UserQuestionRequester> = Arc::new(CancellingRequester);
        let tool = AskUserQuestionTool::new();
        let args = one_valid_question();
        let result = USER_QUESTION_CTX
            .scope(requester_dyn, async move { tool.execute(&args).await })
            .await
            .expect("cancelled is not an error");
        assert!(!result.success);
        let output: Value = serde_json::from_str(&result.output).expect("output json");
        assert_eq!(output["status"], json!("cancelled"));
    }

    #[tokio::test]
    async fn should_reject_when_questions_out_of_range() {
        let tool = AskUserQuestionTool::new();

        // Zero questions.
        assert!(tool.execute(&json!({ "questions": [] })).await.is_err());

        // Five questions.
        let too_many: Vec<Value> = (0..5)
            .map(|i| {
                json!({
                    "header": format!("H{i}"),
                    "question": "q?",
                    "options": [
                        { "label": "a", "description": "" },
                        { "label": "b", "description": "" }
                    ]
                })
            })
            .collect();
        assert!(
            tool.execute(&json!({ "questions": too_many }))
                .await
                .is_err()
        );

        // Only one option (below the 2..=4 floor).
        let bad_options = json!({
            "questions": [
                {
                    "header": "Pick",
                    "question": "q?",
                    "options": [ { "label": "only", "description": "" } ]
                }
            ]
        });
        assert!(tool.execute(&bad_options).await.is_err());

        // Over-long header (> 12 chars).
        let long_header = json!({
            "questions": [
                {
                    "header": "this-header-is-way-too-long",
                    "question": "q?",
                    "options": [
                        { "label": "a", "description": "" },
                        { "label": "b", "description": "" }
                    ]
                }
            ]
        });
        assert!(tool.execute(&long_header).await.is_err());
    }
}
