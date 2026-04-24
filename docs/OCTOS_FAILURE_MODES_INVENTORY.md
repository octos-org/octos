# Octos Failure Modes Inventory

Living catalogue of how the runtime handles each class of failure.
Update this file whenever a new failure class is discovered or an
existing recovery path changes.

## Conventions

- Each row is a single observable failure class.
- "Trigger" is the smallest reproducible signal (error code, log line,
  protocol event) that the failure has occurred.
- "Recovery path" names the runtime component that owns the response
  and the PR/milestone where it shipped.
- "Tested?" links to a test name (or notes a gap).
- "Owner" is the stewarding workstream — usually the originating
  engineer, sometimes a long-lived module.

## Inventory

| Failure class                          | Trigger                                                                                                | Recovery path                                                                                                                                          | Tested?                                                                                                                                       | Owner                  |
| -------------------------------------- | ------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------- |
| Sync tool error                        | Tool returns `ToolResult { success: false, .. }` or panics, surfaced as `Tool` message in the loop      | Tool-use loop in `octos-agent/src/agent/loop_runner.rs` injects the failure as a `Tool` message; the LLM sees it on the next turn                       | Yes — `agent::tests::*tool_error*` plus integration tests in `tools/` modules                                                                  | octos-agent (existing) |
| Spawn_only task `Failed`               | `TaskSupervisor::mark_failed` fires for a `spawn_only` background task                                  | M8.9: `SpawnOnlyFailureSignal` → session actor enqueues a `RecoveryHint` → synthetic `[system-internal]` turn re-engages the LLM with alternatives      | Yes — `task_supervisor::tests::should_emit_failure_signal_*`, `session_actor::tests::supervisor_failure_signal_generates_recovery_actor_message_end_to_end` | M8.9                   |
| Network partition                      | LLM provider request returns `reqwest::Error` (timeout, connection reset)                              | `RetryProvider` exponential backoff (`octos-llm/src/retry.rs`); falls through to `ProviderChain` failover                                                | Yes — retry + chain integration tests in `octos-llm`                                                                                          | octos-llm (existing)   |
| Provider 429 / 5xx                     | HTTP 429 or 5xx surface from any LLM endpoint                                                          | `RetryProvider` retries → `ProviderChain` failover → `AdaptiveRouter` lane scoring + circuit breakers                                                  | Yes — provider chain test suite                                                                                                               | octos-llm (existing)   |
| LLM returns empty content              | Model returns empty `content` and no tool calls                                                        | Loop emits a synthetic stub assistant message; parent flow receives a non-empty terminal reply                                                          | Yes — `agent::tests::stub_*`                                                                                                                  | octos-agent (existing) |
| Silent voice substitution              | OminiX-API silently swaps an unknown voice for the default one                                          | OminiX-API now returns `404` for unknown voices; `fm_tts` pre-validates voice IDs; `task_supervisor` content-checks the produced audio (PR #12, #48, #559) | Yes — fm_tts unit tests + e2e voice-skill specs                                                                                               | voice-skill            |
| Partial file output                    | Spawn_only tool reports success but emits a truncated/empty payload                                     | `task_supervisor` size + content validation rejects the artifact and routes through the `Failed` path so M8.9 picks it up (PR #550, #559)               | Yes — workspace-contract suite                                                                                                                | task-supervisor        |
| Session crash mid-turn                 | Process exits while a turn is in flight                                                                 | M8.6 structured resume pipeline replays the last committed turn from JSONL on next session boot                                                          | Yes — M8.6 resume integration tests                                                                                                           | M8.6                   |
| Overflow history stale                 | Speculative-overflow path consumes a stale window of conversation history                              | In-flight `a89c7fd1` snapshots primary history at speculative-spawn time and pins it for the overflow worker                                            | Pending — tracking in `a89c7fd1`                                                                                                              | runtime                |
| Recovery turn itself fails             | The recovery turn enqueued by M8.9 produces another spawn_only failure                                  | M8.9 caps recovery at one attempt per task; subsequent failure signals for the same `task_id` are silently dropped                                       | Yes — `session_actor::tests::recovery_hint_only_fires_once_for_same_task_id`, `task_supervisor::tests::should_only_emit_failure_signal_once_per_task` | M8.9                   |

## Adding a new failure class

When a new class is discovered:

1. Add a row above with the trigger, recovery path (or "TBD"), and a
   test reference.
2. If recovery is missing, file an issue and link it from this table.
3. After the recovery ships, update the row in the same PR.

This file is the canonical surface area for failure-mode reviews. Skip
no rows — silent partial coverage is what M8.9 was created to close.
