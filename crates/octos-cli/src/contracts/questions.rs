//! Pending structured-user-question store (UPCR-2026-023).
//!
//! Mirrors [`crate::contracts::approvals::PendingApprovalStore`] for the
//! `ask_user_question` flow: a per-`question_id` map holding the originating
//! [`UserQuestionRequestedEvent`] and a oneshot `Sender` the
//! `user_question/respond` handler resolves. The blocked
//! `ask_user_question` tool awaits the matching `Receiver`; turn interrupt
//! drains pending questions exactly like approval cancellation.

use std::collections::HashMap;
use std::sync::RwLock;

use octos_core::SessionKey;
use octos_core::ui_protocol::{
    QuestionId, RpcError, TurnId, UserQuestionAnswer, UserQuestionRequestedEvent,
    UserQuestionRespondParams, UserQuestionRespondResult, methods, rpc_error_codes,
};
use serde_json::json;

/// Resolution delivered to the waiting `ask_user_question` tool when a client
/// answers. A *closed* oneshot (sender dropped) means the question was
/// cancelled (turn interrupt) — the tool treats that as `Cancelled`.
pub(crate) type UserQuestionResolution = Vec<UserQuestionAnswer>;

#[derive(Debug)]
struct QuestionEntry {
    session_id: SessionKey,
    state: QuestionEntryState,
    request: UserQuestionRequestedEvent,
    runtime_resumable: bool,
    response_tx: Option<tokio::sync::oneshot::Sender<UserQuestionResolution>>,
}

#[derive(Debug)]
enum QuestionEntryState {
    Pending,
    Answered,
    /// The server cancelled this question (turn interrupt) before any client
    /// answered. Late `respond` calls return a typed `user_question_stale`.
    Cancelled {
        reason: String,
    },
}

/// One cancelled question surfaced by [`PendingQuestionStore::cancel_pending_for_turn`].
///
/// Mirrors `ui_protocol_approvals::CancelledApproval`. Phase-1 turn-interrupt
/// reuses the terminal `turn/error` path rather than emitting a per-question
/// cancelled wire event, so the production drain discards these; the fields
/// are read by the store tests and by the follow-up wire-emit slice.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct CancelledQuestion {
    pub(crate) question_id: QuestionId,
    pub(crate) turn_id: TurnId,
}

#[derive(Debug, Clone)]
pub(crate) struct QuestionRespondOutcome {
    pub(crate) result: UserQuestionRespondResult,
}

#[derive(Default)]
pub(crate) struct PendingQuestionStore {
    entries: RwLock<HashMap<QuestionId, QuestionEntry>>,
}

impl PendingQuestionStore {
    /// Register a runtime-blocking question and return the oneshot the waiting
    /// tool awaits. Mirrors `PendingApprovalStore::request_runtime`.
    pub(crate) fn request_runtime(
        &self,
        event: UserQuestionRequestedEvent,
    ) -> tokio::sync::oneshot::Receiver<UserQuestionResolution> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut entries = self.entries.write().unwrap_or_else(|p| p.into_inner());
        entries.insert(
            event.question_id.clone(),
            QuestionEntry {
                session_id: event.session_id.clone(),
                state: QuestionEntryState::Pending,
                request: event,
                runtime_resumable: true,
                response_tx: Some(tx),
            },
        );
        rx
    }

    /// Resolve a pending question with the client's answers. Mirrors
    /// `PendingApprovalStore::respond_with_context`: typed errors on
    /// unknown/stale ids; resolves the waiting tool's oneshot at most once.
    pub(crate) fn respond_with_context(
        &self,
        params: &UserQuestionRespondParams,
    ) -> Result<QuestionRespondOutcome, RpcError> {
        let mut entries = self.entries.write().unwrap_or_else(|p| p.into_inner());
        let Some(entry) = entries.get_mut(&params.question_id) else {
            return Err(question_unknown_error(params));
        };

        if entry.session_id != params.session_id {
            return Err(question_unknown_error(params));
        }

        match &entry.state {
            QuestionEntryState::Pending => {
                // Validate the client's answers against the STORED request
                // BEFORE resolving the waiter — a malformed answer (wrong
                // count, an out-of-options label, multiple labels on a
                // single-select, or free text where disallowed) must not flow
                // into the blocked tool. The entry stays `Pending` on
                // rejection so a corrected respond can still resolve it.
                if let Err(reason) = validate_answers(&entry.request.questions, &params.answers) {
                    return Err(question_invalid_error(params, &reason));
                }
                entry.state = QuestionEntryState::Answered;
                let runtime_resumed = entry
                    .response_tx
                    .take()
                    .is_some_and(|tx| tx.send(params.answers.clone()).is_ok());
                Ok(QuestionRespondOutcome {
                    result: UserQuestionRespondResult::accepted_with_runtime_resumed(
                        params.question_id.clone(),
                        entry.runtime_resumable && runtime_resumed,
                    ),
                })
            }
            QuestionEntryState::Answered => Err(question_stale_error(
                params,
                "already_answered",
                &entry.request.turn_id,
            )),
            QuestionEntryState::Cancelled { reason } => {
                Err(question_stale_error(params, reason, &entry.request.turn_id))
            }
        }
    }

    /// Atomically cancel every still-pending question for the given turn.
    /// Idempotent. Mirrors `PendingApprovalStore::cancel_pending_for_turn`.
    pub(crate) fn cancel_pending_for_turn(
        &self,
        session_id: &SessionKey,
        turn_id: &TurnId,
        reason: &str,
    ) -> Vec<CancelledQuestion> {
        let mut entries = self.entries.write().unwrap_or_else(|p| p.into_inner());
        let mut cancelled = Vec::new();
        for (question_id, entry) in entries.iter_mut() {
            if entry.session_id != *session_id {
                continue;
            }
            if !matches!(&entry.state, QuestionEntryState::Pending) {
                continue;
            }
            if entry.request.turn_id != *turn_id {
                continue;
            }
            entry.state = QuestionEntryState::Cancelled {
                reason: reason.to_owned(),
            };
            // Drop the runtime waiter; the blocked tool sees the closed
            // receiver and treats it as Cancelled.
            entry.response_tx = None;
            cancelled.push(CancelledQuestion {
                question_id: question_id.clone(),
                turn_id: entry.request.turn_id.clone(),
            });
        }
        cancelled
    }

    /// Cancel a single pending question (e.g. when the requested notification
    /// failed to send). Mirrors `PendingApprovalStore::cancel_pending_approval`.
    pub(crate) fn cancel_pending_question(
        &self,
        session_id: &SessionKey,
        question_id: &QuestionId,
        reason: &str,
    ) -> Option<CancelledQuestion> {
        let mut entries = self.entries.write().unwrap_or_else(|p| p.into_inner());
        let entry = entries.get_mut(question_id)?;
        if entry.session_id != *session_id {
            return None;
        }
        if !matches!(&entry.state, QuestionEntryState::Pending) {
            return None;
        }
        let turn_id = entry.request.turn_id.clone();
        entry.state = QuestionEntryState::Cancelled {
            reason: reason.to_owned(),
        };
        entry.response_tx = None;
        Some(CancelledQuestion {
            question_id: question_id.clone(),
            turn_id,
        })
    }

    /// Pending requests for reconnect hydration. Mirrors
    /// `PendingApprovalStore::pending_for_session`. Replayed on `session/open`
    /// and `session/hydrate` (gated by `user_question.v1`) so a reconnecting
    /// client re-renders and can still answer a pending question.
    pub(crate) fn pending_for_session(
        &self,
        session_id: &SessionKey,
    ) -> Vec<UserQuestionRequestedEvent> {
        let entries = self.entries.read().unwrap_or_else(|p| p.into_inner());
        entries
            .values()
            .filter(|entry| {
                entry.session_id == *session_id
                    && matches!(&entry.state, QuestionEntryState::Pending)
            })
            .map(|entry| entry.request.clone())
            .collect()
    }
}

/// Validate a `user_question/respond` answer set against the originating
/// request's questions/options (UPCR-2026-023 spec/design contract). Returns a
/// short machine-stable `reason` string on the first violation:
///
/// - exactly one answer entry per question, in order (`answer_count`);
/// - every `selected_labels` value is one of THAT question's option labels
///   (`unknown_label`);
/// - a non-`multi_select` question has at most one selected label
///   (`multi_select_violation`);
/// - `free_text` is present only when the question allows it
///   (`free_text_not_allowed`).
///
/// Mirrors the trust-boundary discipline of the approval `respond` path: the
/// server validates the client payload against server-held state before
/// resolving the runtime waiter, so a bad answer never reaches the tool.
fn validate_answers(
    questions: &[octos_core::ui_protocol::UserQuestion],
    answers: &[UserQuestionAnswer],
) -> Result<(), String> {
    if answers.len() != questions.len() {
        return Err("answer_count".to_owned());
    }
    for (question, answer) in questions.iter().zip(answers.iter()) {
        if !question.multi_select && answer.selected_labels.len() > 1 {
            return Err("multi_select_violation".to_owned());
        }
        for label in &answer.selected_labels {
            if !question.options.iter().any(|opt| &opt.label == label) {
                return Err("unknown_label".to_owned());
            }
        }
        if answer.free_text.is_some() && !question.allow_free_text {
            return Err("free_text_not_allowed".to_owned());
        }
    }
    Ok(())
}

fn question_invalid_error(params: &UserQuestionRespondParams, reason: &str) -> RpcError {
    RpcError::new(
        rpc_error_codes::USER_QUESTION_INVALID,
        "user_question/respond answers do not match the stored question",
    )
    .with_data(json!({
        "kind": "user_question_invalid",
        "method": methods::USER_QUESTION_RESPOND,
        "session_id": params.session_id,
        "question_id": params.question_id,
        "reason": reason,
        "recoverable": true,
    }))
}

fn question_unknown_error(params: &UserQuestionRespondParams) -> RpcError {
    RpcError::new(
        rpc_error_codes::USER_QUESTION_UNKNOWN,
        "user_question/respond target was not found for this session",
    )
    .with_data(json!({
        "kind": "user_question_unknown",
        "method": methods::USER_QUESTION_RESPOND,
        "session_id": params.session_id,
        "question_id": params.question_id,
        "recoverable": false,
    }))
}

fn question_stale_error(
    params: &UserQuestionRespondParams,
    reason: &str,
    turn_id: &TurnId,
) -> RpcError {
    RpcError::new(
        rpc_error_codes::USER_QUESTION_STALE,
        "user_question/respond target is no longer pending",
    )
    .with_data(json!({
        "kind": "user_question_stale",
        "method": methods::USER_QUESTION_RESPOND,
        "session_id": params.session_id,
        "question_id": params.question_id,
        "turn_id": turn_id,
        "reason": reason,
        "recoverable": false,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use octos_core::ui_protocol::{QuestionId, TurnId, UserQuestion, UserQuestionOption};

    fn sample_event(
        session_id: SessionKey,
        question_id: QuestionId,
        turn_id: TurnId,
    ) -> UserQuestionRequestedEvent {
        UserQuestionRequestedEvent::new(
            session_id,
            question_id,
            turn_id,
            "Pick a framework",
            "Which framework should I scaffold?",
            vec![UserQuestion {
                header: "Framework".into(),
                question: "Which framework?".into(),
                options: vec![
                    UserQuestionOption {
                        label: "axum".into(),
                        description: "tower-based".into(),
                    },
                    UserQuestionOption {
                        label: "actix".into(),
                        description: "actor-based".into(),
                    },
                ],
                multi_select: false,
                allow_free_text: true,
            }],
        )
    }

    fn answer(label: &str) -> Vec<UserQuestionAnswer> {
        vec![UserQuestionAnswer {
            selected_labels: vec![label.into()],
            free_text: None,
        }]
    }

    /// A two-option single-select question with free-text DISALLOWED, used to
    /// exercise the answer-validation contract (fix #5).
    fn no_free_text_event(
        session_id: SessionKey,
        question_id: QuestionId,
        turn_id: TurnId,
    ) -> UserQuestionRequestedEvent {
        UserQuestionRequestedEvent::new(
            session_id,
            question_id,
            turn_id,
            "Pick one",
            "Pick exactly one",
            vec![UserQuestion {
                header: "Pick".into(),
                question: "Pick one".into(),
                options: vec![
                    UserQuestionOption {
                        label: "red".into(),
                        description: String::new(),
                    },
                    UserQuestionOption {
                        label: "blue".into(),
                        description: String::new(),
                    },
                ],
                multi_select: false,
                allow_free_text: false,
            }],
        )
    }

    #[tokio::test]
    async fn request_then_respond_resolves_waiting_tool_once() {
        let store = PendingQuestionStore::default();
        let session_id = SessionKey("local:test".into());
        let question_id = QuestionId::new();
        let turn_id = TurnId::new();

        let rx = store.request_runtime(sample_event(
            session_id.clone(),
            question_id.clone(),
            turn_id,
        ));

        let params =
            UserQuestionRespondParams::new(session_id.clone(), question_id.clone(), answer("axum"));
        let outcome = store
            .respond_with_context(&params)
            .expect("pending question accepts");
        assert!(outcome.result.accepted);
        assert!(outcome.result.runtime_resumed);
        assert_eq!(rx.await.expect("answer received"), answer("axum"));

        // Second respond is stale.
        let err = store
            .respond_with_context(&params)
            .expect_err("answered question is stale");
        assert_eq!(err.code, rpc_error_codes::USER_QUESTION_STALE);
        assert_eq!(
            err.data.as_ref().and_then(|d| d.get("kind")),
            Some(&json!("user_question_stale"))
        );
    }

    #[test]
    fn unknown_question_id_returns_typed_error() {
        let store = PendingQuestionStore::default();
        let params = UserQuestionRespondParams::new(
            SessionKey("local:test".into()),
            QuestionId::new(),
            answer("axum"),
        );
        let err = store
            .respond_with_context(&params)
            .expect_err("unknown question id fails");
        assert_eq!(err.code, rpc_error_codes::USER_QUESTION_UNKNOWN);
        assert_eq!(
            err.data.as_ref().and_then(|d| d.get("kind")),
            Some(&json!("user_question_unknown"))
        );
    }

    #[test]
    fn cross_session_respond_is_unknown() {
        let store = PendingQuestionStore::default();
        let session_id = SessionKey("local:test".into());
        let other = SessionKey("local:other".into());
        let question_id = QuestionId::new();
        store.request_runtime(sample_event(
            session_id.clone(),
            question_id.clone(),
            TurnId::new(),
        ));

        let err = store
            .respond_with_context(&UserQuestionRespondParams::new(
                other,
                question_id,
                answer("axum"),
            ))
            .expect_err("question is scoped to its owning session");
        assert_eq!(err.code, rpc_error_codes::USER_QUESTION_UNKNOWN);
    }

    #[tokio::test]
    async fn cancel_on_interrupt_drops_runtime_waiter_and_marks_stale() {
        let store = PendingQuestionStore::default();
        let session_id = SessionKey("local:test".into());
        let question_id = QuestionId::new();
        let turn_id = TurnId::new();
        let rx = store.request_runtime(sample_event(
            session_id.clone(),
            question_id.clone(),
            turn_id.clone(),
        ));

        let cancelled = store.cancel_pending_for_turn(&session_id, &turn_id, "turn_interrupted");
        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0].question_id, question_id);
        assert_eq!(cancelled[0].turn_id, turn_id);

        // The blocked tool sees the receiver close (Cancelled).
        assert!(rx.await.is_err(), "cancel drops the runtime sender");

        // A late respond is stale, not unknown.
        let err = store
            .respond_with_context(&UserQuestionRespondParams::new(
                session_id.clone(),
                question_id,
                answer("axum"),
            ))
            .expect_err("late respond against cancelled question");
        assert_eq!(err.code, rpc_error_codes::USER_QUESTION_STALE);
        assert_eq!(
            err.data.as_ref().unwrap()["reason"],
            json!("turn_interrupted")
        );

        // Idempotent: second cancel is a no-op.
        assert!(
            store
                .cancel_pending_for_turn(&session_id, &turn_id, "turn_interrupted")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn respond_with_wrong_answer_count_is_invalid_and_does_not_resolve() {
        let store = PendingQuestionStore::default();
        let session_id = SessionKey("local:test".into());
        let question_id = QuestionId::new();
        // sample_event has exactly ONE question; supply TWO answers.
        let rx = store.request_runtime(sample_event(
            session_id.clone(),
            question_id.clone(),
            TurnId::new(),
        ));
        let params = UserQuestionRespondParams::new(
            session_id.clone(),
            question_id.clone(),
            vec![
                UserQuestionAnswer {
                    selected_labels: vec!["axum".into()],
                    free_text: None,
                },
                UserQuestionAnswer {
                    selected_labels: vec!["actix".into()],
                    free_text: None,
                },
            ],
        );
        let err = store
            .respond_with_context(&params)
            .expect_err("answer count mismatch must be rejected");
        assert_eq!(err.code, rpc_error_codes::USER_QUESTION_INVALID);
        assert_eq!(
            err.data.as_ref().and_then(|d| d.get("kind")),
            Some(&json!("user_question_invalid"))
        );
        // The waiter is NOT resolved with bad data — it is still pending and a
        // VALID respond still resolves it.
        let good = UserQuestionRespondParams::new(session_id, question_id, answer("axum"));
        store
            .respond_with_context(&good)
            .expect("valid respond after an invalid one still accepts");
        assert_eq!(rx.await.expect("answer received"), answer("axum"));
    }

    #[tokio::test]
    async fn respond_with_label_not_in_options_is_invalid() {
        let store = PendingQuestionStore::default();
        let session_id = SessionKey("local:test".into());
        let question_id = QuestionId::new();
        let rx = store.request_runtime(sample_event(
            session_id.clone(),
            question_id.clone(),
            TurnId::new(),
        ));
        let params = UserQuestionRespondParams::new(session_id, question_id, answer("rocket")); // not an option
        let err = store
            .respond_with_context(&params)
            .expect_err("unknown label must be rejected");
        assert_eq!(err.code, rpc_error_codes::USER_QUESTION_INVALID);
        // Waiter left untouched (closed only on drop), not resolved.
        drop(store);
        assert!(rx.await.is_err(), "waiter must not have been resolved");
    }

    #[tokio::test]
    async fn respond_with_two_labels_on_single_select_is_invalid() {
        let store = PendingQuestionStore::default();
        let session_id = SessionKey("local:test".into());
        let question_id = QuestionId::new();
        store.request_runtime(sample_event(
            session_id.clone(),
            question_id.clone(),
            TurnId::new(),
        ));
        let params = UserQuestionRespondParams::new(
            session_id,
            question_id,
            vec![UserQuestionAnswer {
                selected_labels: vec!["axum".into(), "actix".into()],
                free_text: None,
            }],
        );
        let err = store
            .respond_with_context(&params)
            .expect_err("two labels on a single-select must be rejected");
        assert_eq!(err.code, rpc_error_codes::USER_QUESTION_INVALID);
    }

    #[tokio::test]
    async fn respond_with_free_text_when_disallowed_is_invalid() {
        let store = PendingQuestionStore::default();
        let session_id = SessionKey("local:test".into());
        let question_id = QuestionId::new();
        store.request_runtime(no_free_text_event(
            session_id.clone(),
            question_id.clone(),
            TurnId::new(),
        ));
        let params = UserQuestionRespondParams::new(
            session_id,
            question_id,
            vec![UserQuestionAnswer {
                selected_labels: vec!["red".into()],
                free_text: Some("magenta".into()),
            }],
        );
        let err = store
            .respond_with_context(&params)
            .expect_err("free text on a no-free-text question must be rejected");
        assert_eq!(err.code, rpc_error_codes::USER_QUESTION_INVALID);
    }

    #[tokio::test]
    async fn respond_free_text_only_with_no_labels_is_accepted_when_allowed() {
        // sample_event forces allow_free_text=true; a free-text-only answer
        // (the "Other" escape hatch) with zero selected labels is valid.
        let store = PendingQuestionStore::default();
        let session_id = SessionKey("local:test".into());
        let question_id = QuestionId::new();
        let rx = store.request_runtime(sample_event(
            session_id.clone(),
            question_id.clone(),
            TurnId::new(),
        ));
        let custom = vec![UserQuestionAnswer {
            selected_labels: vec![],
            free_text: Some("rocket".into()),
        }];
        let params = UserQuestionRespondParams::new(session_id, question_id, custom.clone());
        store
            .respond_with_context(&params)
            .expect("free-text-only answer is valid when allow_free_text=true");
        assert_eq!(rx.await.expect("answer received"), custom);
    }

    #[test]
    fn cancelled_question_excluded_from_pending_for_session() {
        let store = PendingQuestionStore::default();
        let session_id = SessionKey("local:test".into());
        let question_id = QuestionId::new();
        let turn_id = TurnId::new();
        store.request_runtime(sample_event(
            session_id.clone(),
            question_id,
            turn_id.clone(),
        ));
        assert_eq!(store.pending_for_session(&session_id).len(), 1);

        store.cancel_pending_for_turn(&session_id, &turn_id, "turn_interrupted");
        assert!(store.pending_for_session(&session_id).is_empty());
    }
}
