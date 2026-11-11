# octos-swarm TESTS-KEPT

Tracking: T-0574. Baseline is octos main `7c16f6c3`: `wake/test-lines.py --json` reports **2,981 test lines / 57 tests** for this crate. Every name below was checked with `cargo test -p octos-swarm --all-targets -- --list`.

Categories: **1** cross-system; **2** cross-feature; **3** authorization/security; **4** recovery/data/concurrency/protocol framing; **5** intentional design. Integration rows use `tests/<target>.rs::<exact libtest name>`. Only representative behavior is retained; topology renames, field mirrors, CRUD permutations, and duplicate outcome cases are excluded.

## Category 3 security subclasses (fixed five-row audit)

| Subclass | Exact test representative, or code-backed reason there is no such boundary |
|---|---|
| 路径穿越 | **本 crate 无此边界。** `DispatchStore::open` accepts a caller-owned data directory and joins only the fixed `swarm-state.redb` filename; contract/task payloads are not resolved as filesystem paths. |
| 沙箱逃逸 | `tests/swarm_dispatch_policy.rs::require_sandboxed_blocks_unsandboxed_backend` — an unsandboxed backend is denied before dispatch when sandboxing is required. |
| 鉴权/权限 | `tests/swarm_dispatch_policy.rs::approval_required_without_requester_fails_closed`; `tests/swarm_dispatch_policy.rs::local_mcp_backend_respects_tool_policy_deny` and `tests/swarm_dispatch_policy.rs::remote_mcp_backend_respects_tool_policy_deny` cover both real backend policy paths. |
| 密钥/令牌脱敏 | **本 crate 无 secret 输出/日志脱敏边界。** It has no secret-specific persisted field or redaction helper. `tests/swarm_dispatch_policy.rs::env_allowlist_blocks_forbidden_keys` is retained separately for forbidden environment injection, not mislabeled as output redaction. |
| 外部输入大小上限 | `dispatcher::tests::budget_clamps_to_max_contracts` — caller-controlled fanout budgets are clamped to `MAX_CONTRACTS_PER_DISPATCH`, preventing unbounded subtask fanout. |

## Retained tests

| Cat. | Exact test | Guard |
|---|---|---|
| 3 | `dispatcher::tests::budget_clamps_to_max_contracts` | External fanout count is bounded even when the requested budget exceeds the protocol cap. |
| 3 | `gate::tests::enforce_or_outcome_folds_tool_policy_denial_into_subtask_outcome` | Policy denial becomes a typed subtask outcome instead of bypassing dispatch accounting. |
| 4 | `persistence::tests::reopening_recovers_prior_rows` | Reopening the real redb store recovers durable dispatch state across supervisor lifetimes. |
| 4 | `result::tests::from_parts_rolls_outcome_to_partial_when_mixed` | Mixed subtask outcomes roll up to partial without hiding failed work. |
| 4 | `result::tests::from_parts_surfaces_aborted_for_sequential_hard_fail` | A sequential terminal failure preserves the aborted aggregate outcome. |
| 2 | `tests/subtask_contracts.rs::should_run_completion_validators_in_swarm_subtask` | Missing required artifacts demote every backend Success to TerminalFailed with the failing validator id in each subtask error. |
| 2 | `tests/subtask_contracts.rs::should_pass_swarm_subtask_when_required_validator_satisfied` | A required completion validator participates in the real subtask success chain. |
| 4 | `tests/subtask_contracts.rs::should_use_reserve_api_for_budget_isolation_in_swarm` | Swarm cost reservation is isolated through the reserve API rather than bypassing budget ownership. |
| 4 | `tests/subtask_contracts.rs::should_commit_reservation_on_swarm_subtask_completion` | Completion commits the same reservation so budget accounting cannot leak or double-charge. |
| 4 | `tests/swarm_dispatch.rs::should_sequence_contracts_in_order_with_abort_on_failure` | Sequential execution stops at the first terminal failure instead of running later contracts. |
| 4 | `tests/swarm_dispatch.rs::should_redispatch_failed_subcontract_bounded_retries` | Retryable failures are redispatched only within the bounded retry budget. |
| 4 | `tests/swarm_dispatch.rs::should_redispatch_recovering_subcontract_within_budget` | A recovering subcontract resumes within budget instead of being stranded or retried forever. |
| 4 | `tests/swarm_dispatch.rs::should_survive_process_restart_mid_dispatch` | A real store reopen after mid-dispatch process loss resumes without duplicating completed work. |
| 4 | `tests/swarm_dispatch.rs::should_emit_typed_swarm_dispatch_event` | The real dispatch chain emits the typed swarm event with its outcome, preserving the event contract. |
| 4 | `tests/swarm_dispatch.rs::should_stop_pipeline_round_at_retryable_stage_and_resume_with_input` | Pipeline retry stops at the failed stage and resumes with the correct prior-stage input. |
| 4 | `tests/swarm_dispatch.rs::should_replay_finalized_result_verbatim_including_validator_verdicts` | Finalized replay returns the persisted result verbatim, including validator verdicts. |
| 4 | `tests/swarm_dispatch.rs::should_error_not_panic_when_resume_contracts_fewer_than_recorded` | A truncated resume contract list fails loudly instead of panicking or dropping recorded work. |
| 4 | `tests/swarm_dispatch.rs::should_error_when_resume_contract_ids_mismatch_recorded` | Resume rejects changed contract ids rather than attaching results to the wrong slots. |
| 4 | `tests/swarm_dispatch.rs::should_error_when_finalized_replay_contracts_mismatch` | Finalized id reuse with different contracts is rejected rather than replaying stale output. |
| 4 | `tests/swarm_dispatch.rs::should_reject_concurrent_dispatch_with_same_id` | Same-id concurrent dispatches cannot double-run or clobber rounds, and the guard releases afterwards. |
| 4 | `tests/swarm_dispatch.rs::should_not_resume_pipeline_tail_after_persisted_terminal_failure` | Crash/cancel recovery never runs a pipeline tail after a persisted terminal stage failure. |
| 4 | `tests/swarm_dispatch.rs::should_not_resume_sequential_tail_after_persisted_terminal_failure` | Reopening a Sequential persisted terminal-failure record never executes the tail; backend history stays empty and the aggregate remains Aborted. |
| 4 | `tests/swarm_dispatch.rs::should_not_run_extra_round_when_resumed_at_retry_cap` | Resume at the exact retry cap cannot burn an extra retry round. |
| 4 | `tests/swarm_dispatch.rs::should_recompute_replay_for_legacy_finalized_record_without_snapshot` | Legacy finalized rows without snapshots still recompute a compatible replay result. |
| 4 | `tests/swarm_dispatch.rs::should_reject_same_id_with_changed_task_payload` | Contract fingerprinting rejects same-id reuse with changed task bytes. |
| 4 | `tests/swarm_dispatch.rs::should_not_burn_retry_budget_on_progress_rounds` | Progress rounds do not consume the terminal-failure retry budget. |
| 1 | `tests/swarm_dispatch.rs::should_dispatch_through_cli_backend_with_retry_on_nonzero_exit` | The real CLI backend maps nonzero exit into bounded retry instead of silently succeeding. |
| 3 | `tests/swarm_dispatch_policy.rs::local_mcp_backend_respects_tool_policy_deny` | Local MCP dispatch enforces tool-policy denial before backend execution. |
| 3 | `tests/swarm_dispatch_policy.rs::remote_mcp_backend_respects_tool_policy_deny` | Remote MCP dispatch enforces the same denial at the remote boundary. |
| 2 | `tests/swarm_dispatch_policy.rs::sequential_aborts_on_gate_denial` | Policy denial composes with Sequential topology: c2 is policy_denied, c3 stays not_run, and the backend executes only c1. |
| 3 | `tests/swarm_dispatch_policy.rs::approval_required_without_requester_fails_closed` | Approval-required dispatch without a requester fails closed. |
| 3 | `tests/swarm_dispatch_policy.rs::approval_deny_blocks_dispatch` | An explicit approval denial prevents backend execution. |
| 3 | `tests/swarm_dispatch_policy.rs::approval_approve_lets_dispatch_through` | The positive approval path dispatches exactly once, preventing an over-broad deny gate. |
| 3 | `tests/swarm_dispatch_policy.rs::env_allowlist_blocks_forbidden_keys` | Forbidden environment injection is denied before either backend can run. |
| 3 | `tests/swarm_dispatch_policy.rs::require_sandboxed_blocks_unsandboxed_backend` | Sandbox-required policy refuses an unsandboxed backend fail-closed. |
| 5 | `tests/swarm_dispatch_policy.rs::no_policy_preserves_legacy_behaviour` | The intentional no-policy compatibility mode preserves legacy dispatch behavior. |

共 **36 项**。

## Cross-cutting audit

| Area | Audit result |
|---|---|
| 跨功能交互 | Required-validator failure/success, reserve/commit budget ownership, local/remote policy gates, and policy×Sequential abort are retained; pure field plumbing and ledger stubs are not. |
| 失败/迟到路径 | Validator-id terminal failures, bounded retries, retry-cap boundaries, terminal-failure no-tail, changed-payload rejection, and finalized mismatch failures are retained. |
| 取消 | No test is named only for cancellation; Pipeline and Sequential persisted terminal-failure tests explicitly cover crash/cancel recovery, while the same-id concurrency test covers the in-flight guard lifecycle. |
| 崩溃恢复 | Mid-process restart, Pipeline and Sequential terminal-failure no-tail, legacy snapshot-less replay, and finalized verbatim replay are distinct retained recovery stages. |
| 数据完整性 | Contract fingerprints, contract-count/id mismatch errors, validator-verdict replay, and mixed/aborted rollups protect against stale or partial data. |
| 并发/锁 | Same-id concurrent dispatch and budget reserve/commit are retained; ordinary store overwrite permutations are not. |
| 精确边界 | Retry-cap and max-contract clamps cover exact upper bounds without duplicating every budget value. |
| 协议帧 | The typed swarm dispatch event is retained; topology labels and per-field event mirrors are excluded. |
| 平台差异 | No swarm-specific OS permission/keychain branch exists. The real CLI backend failure path is retained and left to Windows/macOS CI for host coverage; no live/canary access is needed. |
