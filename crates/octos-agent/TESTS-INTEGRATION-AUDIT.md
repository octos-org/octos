# octos-agent integration deletion audit

> T-0507 narrowed the restoration to 16 integration tests. T-0551 adds only six named security representatives (two path guards, solo permission, bearer comparison, and two hash/signature rejections); the other candidates remain deleted.

- Original: **55 targets / 376 source tests** (including ignored and platform-gated).
- Restored after T-0551: **22 source tests in 6 targets**, plus the main-added `read_image_wire` test for **23 kept integration tests in 7 targets**; **117 entries = 94 lib + 23 integration**. Linux lists 116 because the sole validator test is `#[cfg(windows)]`.
- Not restored: **353 tests**. The 55-target table is deletion evidence, not permission to restore. Additional links require separate lead/shu approval with a concrete reason.

## Target-by-target decision

| Target | Original | Kept | Deletion evidence / scope decision |
|---|---:|---:|---|
| `abi_compat.rs` | 13 | 0 | 0/13: outside the approved 16-test scope. Prior analysis: durable WorkspacePolicy/HookPayload/TaskResult/ProgressEnvelope v1、legacy default、future reject；外部 app-skill ABI，非普通 serde smoke. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `agent_mcp_dispatch_policy.rs` | 3 | 0 | 0/3: outside the approved 16-test scope. Prior analysis: MCP dispatch/helper fanout、deny 与 backend env injection fail-closed；native spawn 不覆盖. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `browser_research.rs` | 3 | 0 | 0/3: 3 项全 ignored，依赖 headless Chrome/live sites，非默认 CI 回归；仅 optional manual soak |
| `child_primitive_contracts.rs` | 9 | 0 | 0/9: outside the approved 16-test scope. Prior analysis: delegate/spawn/MCP child 的 completion validator fail/pass 与 compaction policy propagation；通用 gate 不证明 primitive wiring. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `cli_agent_backend.rs` | 9 | 0 | 0/9: outside the approved 16-test scope. Prior analysis: 真实 Unix child 的 argv/stdin/JSON、nonzero/missing binary、timeout kill、blocked env；独立 CLI backend lane. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `compaction_policy.rs` | 12 | 0 | 0/12: outside the approved 16-test scope. Prior analysis: preflight/artifact invariant fail-closed、typed placeholders/schema、phase events；超出一般 cache/tool-pair 覆盖. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `compaction_wiring.rs` | 3 | 0 | 0/3: outside the approved 16-test scope. Prior analysis: production policy→runner、LLM summarizer selection 与 legacy no-runner wiring. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `concurrent_scheduler.rs` | 5 | 0 | 0/5: outside the approved 16-test scope. Prior analysis: 真实 Agent batch 的 exclusive order、mixed phases、error cascade、tool-call ID 保序；仅 all-safe 一项判重. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `cost_ledger.rs` | 8 | 0 | 0/8: outside the approved 16-test scope. Prior analysis: redb reopen、contract aggregation、typed attribution、dispatch/budget/history；retained corruption 只覆盖异常索引. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `cost_reservation.rs` | 3 | 0 | 0/3: outside the approved 16-test scope. Prior analysis: 并发 budget TOCTOU、drop refund、actual-vs-projected commit. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `dangerous_permissions.rs` | 9 | 1 | T-0551: restore only `dangerous_profile_is_gated_to_solo_runtime`; the other eight permission combinations remain deletion evidence. |
| `delegate_tool.rs` | 10 | 0 | 0/10: outside the approved 16-test scope. Prior analysis: parent blocking、prompt/session context、grandchild depth、delegated deny、artifact gate、event sink. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `dependency_boundary.rs` | 1 | 0 | 0/1: Cargo.toml substring lint，不是 resolved dependency/runtime guard；脆弱且无可观察行为覆盖 |
| `domain_hook.rs` | 6 | 0 | 0/6: outside the approved 16-test scope. Prior analysis: domain enricher/truncation 与真实 POSIX BeforeSpawnVerify allow/deny；example smoke 可删. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `external_job_completion.rs` | 21 | 0 | 0/21: outside the approved 16-test scope. Prior analysis: receipt watch 的 provider/host/profile/hook/approval/budget/session gates、完成/失败/取消/timeout/rebind/symlink 路径. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `fixed_dispatcher.rs` | 13 | 0 | 0/13: outside the approved 16-test scope. Prior analysis: discovery/execution schema、host/hidden/profile/provider boundaries、late plugin、oversized schema；仅 cancel/snapshot 判重. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `harness_errors.rs` | 19 | 0 | 0/19: outside the approved 16-test scope. Prior analysis: LLM/hook error taxonomy、403 quota-vs-auth、event/ABI/metrics/delegate-depth 失败语义. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `human_wait_batch_timeout.rs` | 3 | 0 | 0/3: outside the approved 16-test scope. Prior analysis: human answer 不受普通 batch 1s ceiling 截断，parallel/serial/mixed 均不 detach stale question. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `human_wait_ctx_propagation.rs` | 3 | 0 | 0/3: outside the approved 16-test scope. Prior analysis: parallel user-question、serial approval task-local propagation 与真实 ask_user_question block/answer. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `integration.rs` | 5 | 0 | 0/5: outside the approved 16-test scope. Prior analysis: 仅保留 max-iteration/token-budget 用户可见 stop 契约；其余 response/tool-loop/trim smoke 判重. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `loop_retry_state.rs` | 10 | 0 | 0/10: outside the approved 16-test scope. Prior analysis: retry buckets、hook deny、legacy state、overflow compact、budget grace/fresh history、shell-spiral parity、metrics. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `m8_end_to_end_gate.rs` | 2 | 0 | 0/2: outside the approved 16-test scope. Prior analysis: 完整 resume 与 background hard gate：transcript/cache/refusal 及 disk output/summary/terminal/runtime detail. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `m8_integration_cache_handoff.rs` | 4 | 0 | 0/4: outside the approved 16-test scope. Prior analysis: resume recovered-ref seeding/hashless/mtime safety、tier3 clear/post-clear read；手工 clear 一项判重. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `m8_integration_concurrency.rs` | 8 | 0 | 0/8: outside the approved 16-test scope. Prior analysis: production spawn/task-control/plugin/MCP concurrency declarations；unknown MCP value fail-exclusive. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `m8_integration_subagent_runtime.rs` | 4 | 0 | 0/4: outside the approved 16-test scope. Prior analysis: production output router、watcher start/stop、runtime_detail 与 summarizer；不止 supervisor lifecycle/cancel. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `m8_integration_tool_context.rs` | 4 | 0 | 0/4: outside the approved 16-test scope. Prior analysis: agent_definitions、spawn-only full ToolContext、profile permissions threading；foreground cache 一项判重. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `m8_plugin_v2_contract.rs` | 9 | 0 | 0/9: outside the approved 16-test scope. Prior analysis: plugin v2 progress/cost/failure、unknown fields、legacy/mixed lines、sink→runtime detail、stable schema/state. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `manage_skills_hash.rs` | 5 | 2 | T-0551: restore wrong-signer and substituted-bytes rejection; positive install and missing-proof variants remain outside this scoped follow-up. |
| `mcp_agent_backend.rs` | 8 | 0 | 0/8: outside the approved 16-test scope. Prior analysis: 真实 stdio fake child/loopback HTTP：artifact、redirect、kill/reap、env、timeout、event、context isolation. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `mcp_server.rs` | 13 | 1 | T-0551: restore only bearer parsing plus constant-time comparison; the remaining session server scenarios stay deleted. |
| `multi_model.rs` | 13 | 0 | 0/13: outside the approved 16-test scope. Prior analysis: ProviderRouter+Spawn 的 model/context/tool-policy/schema/prompt；删除宽泛 workflow 与伪 failover. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `pdf_read_safety.rs` | 3 | 0 | 0/3: outside the approved 16-test scope. Prior analysis: 真实 valid PDF extraction、malformed controlled error、recursion fix parser-version lock. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `persist_append_only_log_new16.rs` | 1 | 0 | 0/1: outside the approved 16-test scope. Prior analysis: repair mutation 后 response 仅 current-turn rows，避免旧 JSONL 重持久化. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `read_image_wire.rs` | 1 | 1 | Main 后续已保留真实 read→Agent→provider HTTP JSON 像素与后续 read 历史一致性；非 T-0551 新增。 |
| `realtime_loop.rs` | 7 | 0 | 0/7: outside the approved 16-test scope. Prior analysis: heartbeat cadence/stall abort、sensor budget/truncate/stall degrade、snapshot→hook；example 可删. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `retry_state_persistence.rs` | 4 | 0 | 0/4: outside the approved 16-test scope. Prior analysis: 跨 dispatch counters、无 handle reset、concurrent agents merge；serde 与 loop_retry_state 判重. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `robot_tool_policy.rs` | 5 | 0 | 0/5: outside the approved 16-test scope. Prior analysis: SafetyTier→ToolPolicy observe/motion/full-actuation allow/deny 与 invalid tier；ignored example 可删. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `runtime_http_discovery.rs` | 6 | 0 | 0/6: outside the approved 16-test scope. Prior analysis: startup catalog/tier fallback/override/activate，以及 unreachable/garbage hard-fail 防空 registry. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `security_sandbox.rs` | 24 | 15 | T-0507's 13 kernel tests plus T-0551's `should_reject_absolute_paths` and `should_reject_path_traversal`; the other nine application-level cases stay deleted. |
| `session_summary.rs` | 7 | 0 | 0/7: outside the approved 16-test scope. Prior analysis: typed summary、decision refinement/stale、三失败 extractive fallback、legacy/future schema、byte roundtrip. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `skill_install_with_lifecycle.rs` | 7 | 0 | 0/7: outside the approved 16-test scope. Prior analysis: preflight/init/ready、critical/HTTP abort、HTTP/static discovery、sibling isolation；shutdown 与 CLI 判重. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `slides_validator_project_scope.rs` | 7 | 0 | 0/7: outside the approved 16-test scope. Prior analysis: HTML/PPTX project gate、spawn/MCP ledger、background files_to_send propagation. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `spawn_only_empty_id_recovery.rs` | 1 | 0 | 0/1: outside the approved 16-test scope. Prior analysis: empty tool_call_id→streaming synthesis→background failure→recovery signal 全链. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `spawn_only_handle_envelope.rs` | 2 | 0 | 0/2: outside the approved 16-test scope. Prior analysis: synth Tool message 仅 task_handle、不泄 full output；无 reader legacy fallback. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `spawn_only_interrupt_cancellation.rs` | 2 | 2 | 2/2: retain detached-future drop and router.mark_terminal cancellation teardown. |
| `spawn_only_policy_gate.rs` | 3 | 0 | 0/3: outside the approved 16-test scope. Prior analysis: spawn intercept 前同步 deny、late registration 重应用、stale call 不得 fake async success. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `spawn_only_preflight_chip_clear.rs` | 2 | 0 | 0/2: outside the approved 16-test scope. Prior analysis: preflight/policy early return 仍发同 tool_id ToolCompleted，清 activity chip. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `subagent_output_and_summary.rs` | 1 | 0 | 0/1: outside the approved 16-test scope. Prior analysis: 1MiB routed disk output + 3 summary ticks + SubagentProgress/runtime_detail. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `swarm_mailbox.rs` | 6 | 0 | 0/6: outside the approved 16-test scope. Prior analysis: idle/ack/kind、file tmp→rename、orphan recovery、ack removal、concurrent writers. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `test_hn_fetch.rs` | 1 | 0 | 0/1: ignored live RSS/HTTP 打印探测，外部不稳定且几乎无断言 |
| `test_news_fetch.rs` | 12 | 0 | 0/12: 11个 helper 是测试内复制实现，real news skill 有同名生产单测；live test ignored/打印 |
| `tiered_compaction_loop.rs` | 2 | 0 | 0/2: outside the approved 16-test scope. Prior analysis: 下一 iteration 50KiB result→placeholder；non-Anthropic 省略 tier2 payload. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `tool_capabilities.rs` | 6 | 0 | 0/6: outside the approved 16-test scope. Prior analysis: batch text/image/per-item error、scope/allowlist、web_fetch SSRF/nesting、truncated batch/cache. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `tool_io_parity.rs` | 11 | 0 | 0/11: outside the approved 16-test scope. Prior analysis: PTY sandbox/env/mutation、I/O once、image identity、logs/budget、TTY、descendant kill、pre-yield cancel. That identifies a candidate, not approval to restore; escalate separately if a concrete link cannot be dropped. |
| `validator_runner.rs` | 17 | 1 | 1/17: retain only Windows AppContainer helper-loss fail-closed; the other 16 are outside this PR. |

## Security-only exact exclusions

| Excluded test | Reason |
|---|---|
| `should_allow_relative_paths_within_workspace` | Application-level resolve_path/SSRF/symlink/env/arg-size coverage, not one of the 13 kernel-enforcement tests. |
| `should_normalize_dot_dot_without_escaping` | Application-level resolve_path/SSRF/symlink/env/arg-size coverage, not one of the 13 kernel-enforcement tests. |
| `should_reject_symlink_in_read_no_follow` | Application-level resolve_path/SSRF/symlink/env/arg-size coverage, not one of the 13 kernel-enforcement tests. |
| `should_reject_symlink_in_write_no_follow` | Application-level resolve_path/SSRF/symlink/env/arg-size coverage, not one of the 13 kernel-enforcement tests. |
| `should_block_private_ips` | Application-level resolve_path/SSRF/symlink/env/arg-size coverage, not one of the 13 kernel-enforcement tests. |
| `should_block_private_hosts` | Application-level resolve_path/SSRF/symlink/env/arg-size coverage, not one of the 13 kernel-enforcement tests. |
| `should_have_all_critical_blocked_env_vars` | Application-level resolve_path/SSRF/symlink/env/arg-size coverage, not one of the 13 kernel-enforcement tests. |
| `should_block_dangerous_docker_cwd` | Application-level resolve_path/SSRF/symlink/env/arg-size coverage, not one of the 13 kernel-enforcement tests. |
| `should_enforce_tool_argument_size_limit` | Application-level resolve_path/SSRF/symlink/env/arg-size coverage, not one of the 13 kernel-enforcement tests. |

## Fixtures

None of the 23 retained integration tests uses the 12 deleted fixtures. Those fixtures belonged to ABI/session-summary/PDF targets outside the scoped restorations and remain deleted with their targets.

## Required runtime evidence

- macOS must run all 15 security tests (13 kernel cases plus two path guards) and both cancellation tests on the new head.
- Windows must run `windows_command_validator_fails_closed_when_appcontainer_helper_vanished`; Linux cannot list that cfg-gated test.
- T-0551 growth must remain ≤1,500 lines after shared-helper dedup; a positive delta requires the normal human-maintainer growth label. T-0507's fail-closed target guards remain in force.
