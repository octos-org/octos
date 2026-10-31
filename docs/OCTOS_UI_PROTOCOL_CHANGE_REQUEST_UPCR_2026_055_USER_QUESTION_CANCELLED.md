# UPCR-2026-055 — durable user-question cancellation notification

## Summary

Adds `user_question/cancelled` to the UI Protocol notification family: when a turn with a pending structured `user_question` is interrupted, the kernel emits a durable `UserQuestionCancelled` event (session_id, topic, question_id, turn_id, reason) to the ledger and reconnect replay.

## Motivation

Approvals have had `approval/cancelled` since the start. User questions were added later without the cancellation counterpart: cancelling a pending question only modified in-memory state, so a reconnecting client kept showing a question whose turn was already dead (#1492). The asymmetry also hid the cancellation from the audit ledger.

## Wire contract

- Method: `user_question/cancelled`
- Params: `UserQuestionCancelledEvent` — `session_id`, `topic` (optional), `question_id`, `turn_id`, `reason`.
- Emitted once per cancelled pending question, at the interrupt drain. Idempotent: already-answered or already-cancelled questions are skipped.

## Compatibility

Additive notification. Clients that don't handle it ignore unknown methods; clients that render pending questions should treat this as "remove the question, the turn died".
