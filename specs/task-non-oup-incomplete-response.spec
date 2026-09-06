spec: task
name: "Non-OUP conversational hosts preserve incomplete responses as failures"
tags: [gateway, specialist, usage, recovery]
---

## Contract

The conversational Agent's IncompleteResponseError is not an ordinary success.
Gateway serial and speculative-primary hosts persist the actual earlier tool,
assistant and user rows plus the distinct current partial final. Detached
overflow retains its existing final-only persistence policy, which avoids
concurrent tool-call ID collisions. All hosts fold the partial's accumulated
usage once; they do not add its PartialTurnUsage wrapper a second time.

Partial text, reasoning and artifacts remain real task output. The visible
incomplete notice is a host diagnostic, not a fabricated persisted assistant
answer. Silent cron control text must not suppress the failure notice.
An already streamed overflow bubble is finalized once; the owned durable reply
is fanned out once without closing the primary request's stream.
If the final append fails after streaming, retain the existing bubble's actual
partial/error notice but emit no empty outbound without durable identity. Do
not invent a session_result or close the primary stream for that overflow.

API incomplete completion emits an error terminal with explicit incomplete /
truncated state and retained usage/commit metadata, not a successful done event.
Ordinary successful done output remains compatible. Native specialist execution
keeps failed status while retaining actual partial output/artifacts.

## Regression mapping

- should_preserve_max_tokens_partial_in_serial_gateway_turn
- should_preserve_max_tokens_partial_in_primary_gateway_turn
- should_preserve_max_tokens_partial_in_overflow_gateway_turn
- should_preserve_max_tokens_partial_in_streamed_overflow_without_second_bubble
- should_not_send_empty_overflow_notification_when_partial_persistence_fails
- should_preserve_max_tokens_partial_in_silent_serial_failure_without_fake_answer
- should_preserve_max_tokens_partial_in_failed_native_specialist
- should_close_incomplete_completion_with_error_not_success_done
- should_deliver_incomplete_overflow_once_without_closing_primary_stream

## Scope boundaries

This does not migrate the gateway to OUP, change the separate Agent task-loop
contract, persist overflow intermediate tool rows, or claim that every error /
cancel path has complete usage accounting. FFI/C/UniFFI and JSON local chat have
their own partial-result boundary regressions. Full combined tests, lint and
fixed-binary runtime acceptance are separate from these focused source controls.
