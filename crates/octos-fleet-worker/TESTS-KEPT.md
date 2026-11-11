# octos-fleet-worker TESTS-KEPT

> **阶段 1：仅保留清单。** 本 PR 不删除测试、helper、fixture 或生产代码。必须先经 lead-core 验收、正常维护者评审并实际合入，之后才能另开唯一的 crate-wide 删除 PR。

基线：`main@7c16f6c3b1bff2cec9a64d984d9fd33f0a40cbac`。
`python3 wake/test-lines.py --json`：octos **87,915 行 / 1,994 测试**；本 crate **3,550 行 / 66 测试**。
`cargo +1.97.1 test -p octos-fleet-worker -- --list`：66 个精确名称；基线实跑 **65 passed / 0 failed / 1 ignored**。

类别：**1** 跨系统用户可见链路；**2** 跨功能交互；**3** 安全/授权/边界；**4** 恢复、数据、并发与协议正确性；**5** 有意设计决定。
拟保留 **43 项**，每项均已对照生产调用链；不是按文件名或“可能有用”保留整组。

## 第 3 类：固定五个安全子类

| 安全子类 | 准确测试名，或无边界理由 | 守护的真实行为 |
|---|---|---|
| 路径穿越 | `worker::tests::file_exists_confined_rejects_symlink_escape` | 真实 workspace 文件可读，绝对路径、`..` 和指向 `/etc/passwd` 的 workspace 内 symlink 均拒绝；覆盖 acceptance 的 filesystem confinement。 |
| 沙箱逃逸 | `worker::tests::run_attempt_terminates_when_sandbox_is_not_isolating` | attempt 得到 NoSandbox 时在构建 Agent/LLM 前 Terminate，provider 调用为 0，child 终态 Failed；不会未隔离执行。 |
| 鉴权/权限 | `closed_registry::tests::write_grant_binds_into_worker_registry_and_records_denials` | 真实 registry 的 allowlisted create 成功，非白名单写入与 create-only edit 拒绝且不落地，violation sink 记录 denied 事件。 |
| 密钥/令牌脱敏 | `worker::tests::sandboxed_git_env_is_sanitized_of_secrets` | re-exec 携带 controller `OPENAI_API_KEY`，经 sandboxed git 环境执行 env dump，证明 planted filter 可见环境中无 controller secret；仅覆盖 subprocess 环境继承，不冒充日志或持久化文本脱敏。 |
| 外部输入大小上限 | **本 crate 无此边界。** crate 不解析网络/MCP 帧或原始外部字节流；JSON 工具参数与 MCP schema/frame 上限在 octos-agent，grant 的持久化校验在 octos-fleet。这里只有并发、token、deadline 限额，不能冒充字节上限。 | 不虚构 size 测试；真实 agent/fleet 边界由对应 crate 负责。 |

## 1. 跨系统主链

| 测试名 | 类别 | 守护性质 |
|---|---:|---|
| `pool::tests::dispatch_runs_worker_in_git_worktree_and_keeps_branch` | 1 | pool→worktree→Agent/tool→worker commit→acceptance→fleet branch；checkout 清理但 deliverable branch 与提交保留。 |
| `worker::tests::run_attempt_happy_path` | 1 | run_attempt→Agent→write_file→FileExists acceptance→attempt snapshot/child Success；真实 artifact、token commit 与 reservation release 完整。 |
| `worker::tests::two_dep_chain_promotes_successor_after_completion` | 1 | 前序 run_attempt Accepted 后，store 将依赖 successor 从 Planned 提升为 Ready 并可 fresh launch。 |
| `pool::tests::worktree_deliverable_autocommitted_even_without_worker_commit` | 1 | worker 只写文件不 commit 时，pool 的 sandboxed auto-commit 仍将 deliverable 落到 branch，且 checkout 清理、child Success。 |

## 2. 跨功能交互

| 测试名 | 类别 | 守护性质 |
|---|---:|---|
| `worker::tests::attempt_creates_one_shared_sandbox_instance` | 2 | Agent registry 与 acceptance validator 共用同一 sandbox；factory 每 attempt 仅调用一次，并固定 Unix/Windows command-validator 差异。 |
| `worker::tests::fresh_attempt_after_grant_widen_rebuilds_from_new_grant` | 2 | minimal attempt escalate→Blocked，keeper SetGrant 后 fresh attempt 重建 registry，新增 web_fetch 可用且 escalate 仍存在。 |
| `pool::tests::escalation_to_host_upgrades_next_attempt_to_a_worktree_worker` | 2 | Workspace/scratch attempt escalate 后，keeper 升级 Host，下一 dispatch 按当前 grant 切换到 worktree 并提交 deliverable。 |
| `worker::tests::escalation_wins_regardless_of_turn_end` | 2 | escalation 与 deadline 交互：slot 已设置时即使 turn 超时也 settle Escalated/Blocked，且实际 tokens 不归零。 |
| `pool::tests::escalation_cancel_drop_commits_real_tokens_not_zero` | 2 | pool guard 在 cancel-drop 中保留 pending escalation，提交真实 token、释放 reservation，防止 fresh attempt 重复花费预算。 |

## 3. 其它安全、授权与平台边界

| 测试名 | 类别 | 守护性质 |
|---|---:|---|
| `worker::tests::child_sandboxed_git_env_has_no_secret` | 3 | 上方 re-exec 的 ignored 子进程探针；只有实际执行 env dump 且无 secret 才通过，防止负断言空跑。 |
| `closed_registry::tests::grant_minimal_reproduces_todays_closed_worker` | 3 | registry 实际 get/specs 仅含授权工具与 escalate；peer handoff/gather、message、spawn、human-input 等不可获取，且所有工具不 park。 |
| `closed_registry::tests::closed_worker_shell_refuses_background` | 3 | 真实 shell 工具拒绝 `background:true` 与尾随 `&`，防止 detached child 绕过 attempt deadline/process teardown。 |
| `worker::tests::run_attempt_terminates_when_sandbox_resolution_refuses` | 3 | typed RefusingSandbox 与 NoSandbox 分开覆盖：拒绝后端同样在 Agent 构建前 Terminate，保留 unavailable 原因且 provider 调用为 0。 |
| `pool::tests::dispatch_falls_back_to_scratch_when_network_not_full` | 3 | Host FS + 非 Full network 的混合 grant 不得取得 worktree/repo git write；回退 scratch 且 deliverable 仍可写。 |
| `pool::tests::worktree_terminated_when_backend_lacks_full_fs_write` | 3 | boot 声称支持但 attempt sandbox 运行时降级时，worktree attempt fail-closed，不在不可写后端执行 git deliverable。 |
| `pool::tests::worktree_terminated_when_backend_cannot_run_posix_sh` | 3 | full-FS 但不支持 POSIX sh 的平台/backend 组合直接 Terminate，防止 Windows `cmd /C` 错解 POSIX populate/commit 命令。 |
| `worker::tests::grant_threads_network_and_repo_git_dir_into_sandbox` | 3 | 精确记录每次 attempt 的 SandboxGrant：minimal 无网络、Full scratch 有网络但无 `.git` write，只有实际 worktree 才取得 repo_git_dir。 |
| `closed_registry::tests::grant_unknown_tool_is_rejected_at_build` | 3 | worker factory 实际调用 grant.validate，unknown tool 在任何 live worker 构建前 fail-closed；不是仅验证 fleet 层字段。 |
| `escalate::tests::escalate_requires_a_reason` | 3 | 真实 EscalateTool.execute 缺少 reason 时失败且 slot 保持空，非法 escalation 不产生后续状态副作用。 |
| `worker::tests::run_attempt_rejects_unsupported_validator_ref` | 3 | 执行器无法验证的 ValidatorRef 不能凭 Agent 自报成功通过 acceptance，必须 Rejected/Failed。 |

## 4. 恢复、数据、并发与精确行为

| 测试名 | 类别 | 守护性质 |
|---|---:|---|
| `worker::tests::run_attempt_rejects_when_acceptance_fails` | 4 | Agent turn 成功但 acceptance 失败时，result snapshot 记录失败与原因，child 不误标 Success。 |
| `worker::tests::acceptance_command_kills_subprocess_past_deadline` | 4 | validator 超过 deadline 时进程组被杀死；等待超过脚本 sleep 后仍无迟到 marker，证明不是仅 drop future。 |
| `worker::tests::run_attempt_terminates_on_deadline` | 4 | Agent/provider 超时进入 Terminated，snapshot 含 deadline 原因且 child Failed。 |
| `worker::tests::run_attempt_aborts_on_lost_mark_running` | 4 | mark_running CAS 丢失时返回 Aborted，不写 result snapshot，也不覆盖已由他方推进的 Running attempt。 |
| `worker::tests::escalation_skips_acceptance_no_validator_side_effect` | 4 | escalation 在 acceptance 前短路，CommandExit validator 不执行，marker 永不落地。 |
| `pool::tests::interrupted_attempt_ends_terminal_not_stuck_launching` | 4 | 运行中 abort 经 LaunchGuard 进入终态 Failed，不遗留 Launching/Running child。 |
| `pool::tests::pre_poll_abort_does_not_leak_launching_child` | 4 | spawned future 尚未首次 poll 就 abort 时，预先 armed 的 guard 仍完成 cleanup。 |
| `worker::tests::escalation_settle_never_zero_when_tokens_used` | 4 | launch reservation 为 0 的精确角落中，escalation 仍从 shared tracker 提交真实 usage；不能释放 0 reservation 却不记账而造成重复花费。 |
| `pool::tests::record_error_keeps_guard_armed_others_disarm` | 4 | RecordError 保持 guard armed 以便 Drop 清理；Completed/Superseded/Aborted 才 disarm，覆盖基础设施失败专属映射。 |
| `pool::tests::rejected_after_prep_leaves_no_orphaned_checkout` | 4 | worktree prep 后 launch 以 RejectedNotReady/no-handle 拒绝时，prep→launch rollback 清除 checkout，保留 branch。 |
| `pool::tests::grant_narrowed_during_launch_is_rejected` | 4 | 名称虽为 grant narrowed，实际用连续 Retitle 改 revision：slow prep 与 replan 竞态由 CAS 拒绝，且 checkout 回滚；不冒充真实 grant mutation。 |
| `pool::tests::dispatch_reconciles_preexisting_worktree_on_relaunch` | 4 | 崩溃遗留 branch+checkout 时，relaunch reconcile 而非冲突失败；worker commit 正常落 branch。 |
| `pool::tests::interrupted_worktree_attempt_removes_checkout_keeps_branch` | 4 | worktree attempt 中断后 child Failed、checkout 清除、branch 保留供恢复，不遗留锁住的 checkout。 |
| `pool::tests::worktree_empty_branch_is_not_marked_succeeded` | 4 | 无 acceptance 且 worker 无产出时，empty branch 被降级 Rejected/Failed，不制造空成功。 |
| `pool::tests::concurrent_dispatch_of_one_task_does_not_corrupt_the_worktree` | 4 | 同 task 并发 dispatch 经 per-task lock 仅一个 launch，deliverable 完整，不发生 double prep/reconcile。 |
| `pool::tests::pool_bounds_concurrency` | 4 | 五个真实并发 attempt 的全局限额为 2，且确实发生并行，不是仅靠串行通过上限断言。 |
| `pool::tests::zero_concurrency_is_clamped_not_hung` | 4 | global/per-fleet 都是 0 时钳制为 1，launch 后能完成而非永久等待 permit。 |
| `pool::tests::dispatch_preflight_rejects_bad_workspace_root_without_launching` | 4 | workspace_root 是文件时 preflight 在 launch_child 前失败，child 保持 Ready，不产生 durable wedge。 |
| `pool::tests::hung_deliverable_commit_is_killed_at_deadline` | 4 | planted clean filter 挂起 git add 时，deliverable commit 受 deadline 限制并 Terminate，不永久持有 pool permit。 |

## 5. 有意设计决定

无单独保留项。最小 grant、Blocked 非终态、Hosts/Full 差异等设计决定均由上面的真实安全行为或跨功能链路证明，不为设计声明或字段形状另留测试。

## 逐项核问

| 核问 | 清单回答 |
|---|---|
| 跨功能交互 | grant widen→registry rebuild、escalation→worktree、sandbox→Agent/validator、escalation→deadline/budget 均有第 2 类真实链。 |
| 失败/迟到路径 | acceptance failure、unsupported ValidatorRef、mark_running CAS 失败、escalation 跳过 validator、迟到 marker 不落地均有明确断言。 |
| 取消 | Agent deadline、运行中 abort、pre-poll abort、validator process-group kill、cancel-drop escalation/token settle 分别覆盖。 |
| 崩溃恢复 | preexisting worktree reconcile、interrupted checkout cleanup、RecordError guard cleanup 覆盖；无共享部署/live canary 依赖。 |
| 数据完整性 | snapshot/result、token reservation/commit、deliverable branch、empty-branch downgrade、revision CAS 均验证。 |
| 并发/锁 | per-task preflight lock、global semaphore、concurrent replan CAS 覆盖；无跨 crate 锁改动。 |
| 精确边界 | create-only/write allowlist、zero concurrency clamp、unsupported validator fail-closed、deadline 均保留；没有伪造字节 size 边界。 |
| 协议帧 | 本 crate 无 JSON-RPC/MCP frame codec；typed tool JSON 由 octos-agent 执行，fleet records 由 octos-fleet 校验，因此不保留 parser/字段形状测试冒充帧覆盖。 |
| 平台差异 | POSIX-only validator/worktree 链保留 `cfg(unix)`；no-POSIX-sh backend 与 Windows command-validator 预期单独覆盖。Marker/Restricted sandbox 委托 NoSandbox，只证明路由、gate 与 cleanup，不冒充真实 bwrap/macOS/AppContainer 隔离。 |
| peer/session 授权 | 本 crate 不拥有 peer session；minimal registry 只证明 peer/message/spawn/human-input 工具不可达与后台 shell 拒绝，不冒充 peer token/session 认证。真正 keeper approval/foreign-binding 授权位于 octos-cli，本 crate 无该测试边界。 |

## 审计确认的边界缺口（不以弱测试冒充覆盖）

- peer escalation 的 keeper approval/denial 与 foreign-binding 授权位于 octos-cli，不在本 crate；这里仅保留工具不可达性，不新增跨 crate 测试。
- escalation `reason` 会原样进入 slot/persistent record，本 crate 没有日志或持久化文本脱敏保证；固定 secret 行只证明 git subprocess 的环境清洗。
- escalation slot 的 last-write-wins 与 mutex poisoning 没有直接并发测试；现有成功/取消链不声称覆盖该缺口。
- `web_fetch` host allowlist 的真实拒绝行为属于 octos-agent；本 crate 的字段转发测试被删除，但没有据此声称已有行为覆盖。
- worktree/sandbox marker 证明路由与 fail-closed gate，不是实际 OS backend confinement；真实 backend 属于 octos-agent/系统级测试。

## 有意不保留的 23 项（仅在清单合入后删除）

| 测试名 | 审计理由 |
|---|---|
| `closed_registry::tests::escalate_tool_is_always_available_even_at_minimal_grant` | escalate valve 已由 minimal registry 与 fresh grant-widen 链覆盖。 |
| `closed_registry::tests::grant_expands_tools_and_scopes` | 字段/集合断言；实际 grant 生效由 write fence、fresh rebuild 与 worktree upgrade 覆盖。 |
| `closed_registry::tests::grant_full_enables_raw_network` | 仅检查 grant 字段，未执行网络行为；由 coherent-grant scratch/worktree 链代表。 |
| `closed_registry::tests::grant_hosts_allowlist_enforced_on_web_tool` | host allowlist 字段转交，真实 web 工具约束属于 octos-agent；不为配置转发保留。 |
| `closed_registry::tests::with_builtins_registry_would_contain_ask_user_question` | 只是 closed-registry 测试的 discriminator，不是 worker 场景。 |
| `escalate::tests::escalate_records_request_into_the_slot_and_does_not_park` | 独立工具 slot 断言由 run_attempt/pool 的真实 escalate→Blocked→SetGrant 链覆盖。 |
| `escalate::tests::parse_requested_grant_defaults_to_minimal_when_absent` | advisory parser 默认值，keeper 的真实 grant 才决定权限；不保留字段测试。 |
| `escalate::tests::parse_requested_grant_reads_full_and_host` | advisory parser 字段映射，实际 widen 行为已由 fresh attempt 覆盖。 |
| `pool::tests::dispatch_double_launch_is_rejected` | 更强的同 task 并发 worktree dispatch 已证明仅一个 launch 且 deliverable 不损坏。 |
| `pool::tests::dispatch_falls_back_to_scratch_when_root_is_not_a_git_repo` | 简单环境 fallback；真实 worktree/scratch 与 grant/backend 边界已保留。 |
| `pool::tests::dispatch_falls_back_to_scratch_when_task_not_host_granted` | escalation_to_host 的第一段已证明 Workspace attempt 不创建 fleet branch。 |
| `pool::tests::dispatch_not_ready_task_is_rejected` | not-ready 调度由 octos-fleet ready/CAS 覆盖；worker 保留依赖完成后真实 promotion。 |
| `pool::tests::escalation_survives_a_cancel_via_the_guard` | cancel-drop token 测试同时断言 Blocked/pending/reservation，覆盖同一 guard 路径且更完整。 |
| `pool::tests::scratch_fallback_when_backend_cannot_support_full_fs_write` | boot fallback 不如 attempt 运行时降级 fail-closed 关键；保留后者。 |
| `tests::sandbox_factory_is_threaded_not_defaulted` | Marker/factory 计数由 attempt 的 Agent+validator shared-sandbox 链覆盖。 |
| `tests::tool_timeouts_are_clamped_to_the_deadline` | config 数值断言由 Agent、validator 与 commit 的真实 deadline 行为覆盖。 |
| `worker::tests::acceptance_command_terminates_past_deadline` | validator subprocess kill 测试同时断言 Terminated 且无迟到 marker，覆盖更强。 |
| `worker::tests::confined_relative_accepts_workspace_paths_and_rejects_escapes` | lexical helper 与 file_exists_confined 的绝对/`..` 检查重复，后者还覆盖 symlink。 |
| `worker::tests::escalate_records_request_and_yields_attempt` | fresh grant-widen 已覆盖 escalate→Blocked→fresh launch，cancel/drop 另覆盖 token。 |
| `worker::tests::escalation_floor_never_loses_real_usage_uses_reservation_only_when_absent` | 纯 floor 函数映射由真实 run/pool token settle 覆盖。 |
| `worker::tests::failed_populate_fails_the_attempt` | 内部 populate 单点错误；保留 preflight 失败不 launch 与 reconcile/rollback 的系统路径。 |
| `worker::tests::run_attempt_rejects_out_of_workspace_file_exists` | file_exists_confined 已覆盖同一 absolute-path refusal，并增加 `..` 与 symlink。 |
| `worker::tests::run_sandboxed_git_gates_exit_status_when_required` | 内部 helper 的 require_success 开关，属于单模块分支；不再单独保留该低层矩阵，populate/deliverable 的系统行为仍由保留链覆盖。 |

## 阶段 1 验证

- `python3 wake/test-lines.py --json`：上述基线已保存；删除 PR 必须给同口径删后值。
- `cargo +1.97.1 test -p octos-fleet-worker -- --list`：66 项，保留 43 项与拟删 23 项互斥且完备。
- `cargo +1.97.1 test -p octos-fleet-worker`：**65 passed / 0 failed / 1 ignored**。
- 本 PR 只新增本文件；required job 名、check-arm、通用 selector、生产代码和测试均未改。
