//! Pending structured-user-question store (UPCR-2026-023).
//!
//! Mirrors [`crate::contracts::approvals::PendingApprovalStore`] for the
//! `ask_user_question` flow: a per-`question_id` map holding the originating
//! [`UserQuestionRequestedEvent`] and a oneshot `Sender` the
//! `user_question/respond` handler resolves. The blocked
//! `ask_user_question` tool awaits the matching `Receiver`; turn interrupt
//! drains pending questions exactly like approval cancellation.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, RwLock};

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
    /// The UI Protocol connection whose turn asked (its `ConnectionId`), when
    /// known. See `PendingApprovalStore::pending_owner`.
    owner_connection: Option<u64>,
    /// Asked by an external client's turn (UPCR-2026-036): only
    /// `owner_connection` sees or answers it.
    external: bool,
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
/// The caller emits a durable `user_question/cancelled` for each returned
/// entry. Already answered/cancelled entries are omitted, so concurrent
/// drains and waiter drops emit at most once.
#[derive(Debug, Clone)]
pub(crate) struct CancelledQuestion {
    pub(crate) question_id: QuestionId,
    pub(crate) turn_id: TurnId,
}

#[derive(Debug, Clone)]
pub(crate) struct QuestionRespondOutcome {
    pub(crate) result: UserQuestionRespondResult,
}

const TERMINAL_QUESTION_CAP: usize = 1024;

#[derive(Default)]
pub(crate) struct PendingQuestionStore {
    entries: RwLock<HashMap<QuestionId, QuestionEntry>>,
    /// FIFO of terminal ids used only for bounded late-response errors.
    /// Pending questions are never evicted.
    terminal_order: Mutex<VecDeque<QuestionId>>,
}

impl PendingQuestionStore {
    fn record_terminal(
        &self,
        entries: &mut HashMap<QuestionId, QuestionEntry>,
        question_id: QuestionId,
    ) {
        let mut order = self
            .terminal_order
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        order.push_back(question_id);
        while order.len() > TERMINAL_QUESTION_CAP {
            let Some(expired) = order.pop_front() else {
                break;
            };
            if entries.get(&expired).is_some_and(|entry| {
                matches!(
                    entry.state,
                    QuestionEntryState::Answered | QuestionEntryState::Cancelled { .. }
                )
            }) {
                entries.remove(&expired);
            }
        }
    }

    /// Register a runtime-blocking question and return the oneshot the waiting
    /// tool awaits. Mirrors `PendingApprovalStore::request_runtime`.
    #[cfg(test)]
    pub(crate) fn request_runtime(
        &self,
        event: UserQuestionRequestedEvent,
    ) -> tokio::sync::oneshot::Receiver<UserQuestionResolution> {
        self.request_runtime_owned(event, None)
    }

    /// [`Self::request_runtime`], recording the connection that owns the
    /// question (see [`Self::pending_owner`]).
    pub(crate) fn request_runtime_owned(
        &self,
        event: UserQuestionRequestedEvent,
        owner_connection: Option<u64>,
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
                owner_connection,
                external: false,
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
                let outcome = QuestionRespondOutcome {
                    result: UserQuestionRespondResult::accepted_with_runtime_resumed(
                        params.question_id.clone(),
                        entry.runtime_resumable && runtime_resumed,
                    ),
                };
                self.record_terminal(&mut entries, params.question_id.clone());
                Ok(outcome)
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
        for cancelled_question in &cancelled {
            self.record_terminal(&mut entries, cancelled_question.question_id.clone());
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
        let cancelled = CancelledQuestion {
            question_id: question_id.clone(),
            turn_id,
        };
        self.record_terminal(&mut entries, question_id.clone());
        Some(cancelled)
    }

    /// Pending requests for reconnect hydration. Mirrors
    /// `PendingApprovalStore::pending_for_session`. Replayed on `session/open`
    /// and `session/hydrate` (gated by `user_question.v1`) so a reconnecting
    /// client re-renders and can still answer a pending question.
    /// The owning connection of a PENDING question of `session_id`: `None`
    /// when no such question is pending, `Some(None)` when it has no recorded
    /// owner.
    pub(crate) fn pending_owner(
        &self,
        session_id: &SessionKey,
        question_id: &QuestionId,
    ) -> Option<Option<u64>> {
        let entries = self.entries.read().unwrap_or_else(|p| p.into_inner());
        entries
            .get(question_id)
            .filter(|entry| {
                entry.session_id == *session_id
                    && matches!(&entry.state, QuestionEntryState::Pending)
            })
            .map(|entry| entry.owner_connection)
    }

    /// Mark question `question_id` as asked by an external client's turn
    /// (UPCR-2026-036): see [`Self::external_owner`].
    pub(crate) fn mark_external(&self, question_id: &QuestionId) {
        let mut entries = self.entries.write().unwrap_or_else(|p| p.into_inner());
        if let Some(entry) = entries.get_mut(question_id) {
            entry.external = true;
        }
    }

    /// For a question asked by an external client's turn (in any state):
    /// `Some(owning connection)`, which alone may see or answer it. `None`
    /// for every other question.
    pub(crate) fn external_owner(&self, question_id: &QuestionId) -> Option<Option<u64>> {
        let entries = self.entries.read().unwrap_or_else(|p| p.into_inner());
        entries
            .get(question_id)
            .filter(|entry| entry.external)
            .map(|entry| entry.owner_connection)
    }

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
