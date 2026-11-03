# Octos UI Protocol Change Request: Native Worker Continuation Receipts

## Header

- Request id: `UPCR-2026-065`
- Date: 2026-10-30
- Target protocol: `octos-ui/v1alpha1`
- Status: implemented
- Scope: native worker continuation metadata and missing-executor errors

## Problem

A native worker feedback receipt could record input without delivering it to a
running worker or starting a continuation. The generic task restart path could
also allocate a successor without an attached executor. In both cases callers
could mistake an acknowledged request for work that was actually scheduled.

## Contract

Native `send_input` and `resume_agent` receipts distinguish queued feedback from
completed work. A continuation returns `resumed_agent_id` for its new execution.
The previous execution and its child stream remain terminal; the successor
carries `relaunched_from`. Native task projection metadata and worker status use
`context_session_key` to identify the retained logical context across executions.
Clients must follow the returned execution ID rather than reopening a terminal
stream or treating successful queueing as successful task completion.

Persisted native task metadata alone is insufficient to resume a worker: an
attached executor is required. The generic task restart endpoint likewise checks
for an executor before allocating a successor. If none exists, it returns REST
503 or an OUP runtime error whose data contains
`{"kind":"task_executor_unavailable"}`. No phantom successor is created.

This change uses the existing OUP commands, status values, metadata containers,
and runtime error channel. It does not introduce a new OUP command or status.
Peer input keeps its existing session and durable queue semantics.

The opt-in evidence continuation observer can return `[needs_replan]` after a
measured stall and a correction opportunity. This uses the existing worker
failure/handoff path; it is not a new successful terminal status, and does not
claim that an artifact passed visual review.

## References

- [UI protocol specification](../api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md)
- [Evidence continuation behavior](evidence-continuation.md)
- [OUP transport implementation](../crates/octos-cli/src/api/ui_protocol_transport.rs)
- [Child stream projection](../crates/octos-cli/src/api/ui_protocol_progress.rs)

## Tests

- `restart_task_from_node_without_executor_returns_503_without_successor`
- `appui_restart_without_executor_returns_error_without_phantom_task`
- `appui_task_restart_from_node_uses_relaunch_path`
- `should_deliver_native_feedback_to_running_child_prompt`
- `should_repair_actual_artifact_through_native_resume_with_same_permissions`
- `should_reject_cross_session_and_cancelled_native_feedback`
- `should_refuse_native_resume_after_budget_boundary`

The native worker tests exercise real transcript continuation, artifact repair,
and permission/budget boundaries; the endpoint tests cover the missing-executor
error and absence of a successor.
