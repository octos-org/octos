fn cancel_connection_turn_prompts(
    session_id: &SessionKey,
    turn_id: &TurnId,
    ledger: &UiProtocolLedger,
    approvals: &PendingApprovalStore,
    user_questions: &PendingQuestionStore,
) {
    // #920.2: cancel every still-pending approval for the aborted
    // turn and append a durable `approval/cancelled` for each so a
    // reconnect doesn't re-show a modal for a dead turn.
    let cancelled = approvals.cancel_pending_for_turn(
        session_id,
        turn_id,
        approval_cancelled_reasons::TURN_INTERRUPTED,
    );
    for entry in cancelled {
        if let Err(error) = ledger.try_append_notification(UiNotification::ApprovalCancelled(
            ApprovalCancelledEvent {
                session_id: session_id.clone(),
                topic: session_id.topic().map(ToOwned::to_owned),
                approval_id: entry.approval_id,
                turn_id: entry.turn_id,
                reason: approval_cancelled_reasons::TURN_INTERRUPTED.to_owned(),
            },
        )) {
            warn_ledger_failure("approval cancellation", &error);
        }
    }
    // UPCR-2026-023: also drain pending structured user-questions for the
    // aborted turn so a reconnect doesn't re-show a question for a dead
    // turn and the blocked `ask_user_question` tool unblocks (Cancelled).
    for question in user_questions.cancel_pending_for_turn(
        session_id,
        turn_id,
        user_question_cancelled_reasons::TURN_INTERRUPTED,
    ) {
        if let Err(error) =
            ledger.try_append_notification(UiNotification::UserQuestionCancelled(
                UserQuestionCancelledEvent::turn_interrupted(
                    session_id.clone(),
                    question.question_id,
                    question.turn_id,
                ),
            ))
        {
            warn_ledger_failure("interrupted question cancellation", &error);
        }
    }
}
