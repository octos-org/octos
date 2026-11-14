# octos-agent TESTS-KEPT

保留清单（T-0506 / #3069）：按守护性质分五类。类别：1=跨系统（Agent→工具→结果链路）、2=跨功能（多模块协作）、3=安全（沙箱/权限/脱敏/SSRF/注入）、4=恢复/数据/并发/协议帧、5=有意设计决定。

实测基线：T-0551 后静态清单 **117 = 94 lib + 23 integration**（含 main 后续新增的 13 个 lib 与 `read_image_wire` 1 个 integration 覆盖补录；本 PR 自身只新增 1 个 lib 与 6 个 integration）。Linux 实际列出 116，因为 `tests/validator_runner.rs` 的唯一测试为 `#[cfg(windows)]`。其余 354 项的逐 target 删除证据见 [TESTS-INTEGRATION-AUDIT.md](TESTS-INTEGRATION-AUDIT.md)；不得以 lib 集合代替 integration 审计。固定类别3五行核问直接列出唯一测试名，integration 表不再重复这些行。

## T-0581 第二轮：同链路合并与共享 setup

当前静态清单 **114 = 92 lib + 22 integration**；Linux 实际 113，唯一缺席仍为 Windows validator。只处理上述第一轮保留项，不恢复已删测试。三项同链路合并均先迁入全部探针和断言，再删除重复测试名：

| 合并前 | 合并后 | 保留的完整性质 |
|---|---|---|
| `inject_synthetic_results_pushes_assistant_then_tool_for_every_call` + `inject_synthetic_results_sanitizes_tool_call_ids_with_colons` | `agent::loop_runner::tests::inject_synthetic_results_preserves_pairing_and_sanitizes_colon_ids` | 同一 response 三个调用；assistant/tool 数量与角色、call_a/call_b 精确 ID、primary warning/hint、两个 companion、第三个冒号 ID 的消毒与 1:1 配对全部保留。 |
| `mcp::wire_tests::normal_mcp_call_sends_no_cancellation` + `turn_cancel_sends_cancelled_notification_for_in_flight_mcp_call` | `mcp::wire_tests::turn_cancel_sends_cancelled_notification_for_in_flight_mcp_call` |  fresh pair 正常 baseline（LATE_RESULT/零取消）→ released=0 的 in-flight cancel/request-id/诚实文本 → late result 不改写 → 同连接正常 follow-up/无新增取消，全链保留。 |
| `tests/security_sandbox.rs::should_block_cross_user_write` + `should_isolate_two_users_simultaneously` | `tests/security_sandbox.rs::should_isolate_two_users_simultaneously` | A/B 各自 workspace 写入控制、A→B 非零拒绝及 permission stderr 全部保留；两个 macOS cross-user 探针不再重复。 |

共享 setup（均只在测试内，无生产接口改动）：

- `tests/security_sandbox.rs::SandboxWorkspace::{new,path,run}`：统一 macOS skip、TempDir、canonical workspace、标准 SBPL 与命令执行；自定义 profile、Python、网络和双用户场景仍各自显式。
- `tests/spawn_only_interrupt_cancellation.rs::SpawnOnlyHarness::{new,launch,cancel_and_wait}`：共享 HangingTool/registry/memory/LLM/router/task-id 与 bounded cancel；unrouted future-drop 和 routed terminal teardown 仍独立测试。
- `agent::execution::tests::no_chat_agent`：共享 EpisodeStore/Agent 构造；各 registry、config、cancel token 和错误/租约断言不合并。
- `agent::loop_runner::tests::{end_turn_agent,task_for}`：共享 contract pass/fail 的 agent/task 构造，两个 TempDir、workspace fixture 和结果断言保持独立。
- `tools::spawn::tests::background_spawn_fixture`：共享 SpawnTool/store/supervisor/mpsc 构造并保留 receiver 生命周期；lifecycle 与 fanout-cap 场景不合并。
- `plugins::tool::tests::{input_path_def,write_test_script}`：复用单字符串 schema 和真实进程脚本写入/chmod，input/output symlink 与 model-media 边界不合并。

 oversized-frame 三层、固定五安全子类、取消/超时/迟到、恢复、平台差异均逐项仍在；没有为行数合并不同错误形状、授权结果或精确边界。


## 1. 跨系统（Agent→工具→结果）

| 测试名 | 类别 | 守护性质 |
|---|---|---|
| agent::loop_runner::tests::process_message_preserves_tool_pair_order_across_iterations | 1 | 跨迭代工具调用/结果配对不错乱 |
| agent::loop_runner::tests::inject_synthetic_results_preserves_pairing_and_sanitizes_colon_ids | 1 | 三调用合成结果保持 assistant→tool 配对、primary/companion 语义及冒号 ID 消毒 |
| agent::loop_runner::tests::fixed_dispatcher_keeps_terminal_native_failure_across_entry_points | 1 | 固定分发器跨入口保留原生失败终态 |
| agent::loop_runner::tests::run_task_demotes_success_when_contract_fails | 1 | 契约失败时 run_task 降级 success |
| agent::loop_runner::tests::run_task_keeps_success_when_contract_passes | 1 | 契约通过时保持 success |
| agent::execution::tests::mixed_batch_runs_safe_calls_in_parallel | 1 | 安全调用并行执行 |
| agent::execution::tests::mixed_batch_safe_error_cancels_every_exclusive_call | 1 | 安全错误取消所有排他调用 |
| agent::execution::tests::hard_error_still_cancels_sibling | 1 | 硬错误取消兄弟调用 |
| agent::execution::tests::input_error_does_not_cancel_well_formed_sibling | 1 | 输入错误不误取消正常兄弟 |
| agent::execution::tests::should_not_wait_forever_for_blocking_task_after_abort | 1 | 中止后永不永久等待阻塞任务 |
| tools::spawn::tests::test_background_spawn_tracks_supervisor_lifecycle | 1 | 后台 spawn 跟踪 supervisor 生命周期 |
| tools::spawn::tests::test_background_spawn_refused_at_fanout_cap_does_not_spawn | 1 | fanout 上限拒绝时不产生 spawn |
| tools::spawn::tests::should_deliver_native_feedback_to_running_child_prompt | 1 | 原生反馈送达运行中子 prompt |
| tools::spawn::tests::should_reject_cross_session_and_cancelled_native_feedback | 1 | 拒绝跨会话/已取消的原生反馈 |
| tools::spawn::tests::should_continue_feedback_queued_during_native_terminal_delivery | 1 | 终态投递期间排队的反馈继续处理 |
| tools::spawn::tests::should_allow_nested_native_worker_to_send_feedback_to_its_child | 1 | 嵌套原生 worker 可向其子发反馈 |
| native_subagent::tests::should_not_lose_feedback_admitted_across_native_completion | 1 | 原生完成边界不丢失已准入反馈 |
| native_subagent::failed_handoff_tests::should_report_failed_native_handoff_instead_of_completed_predecessor | 1 | 原生移交失败不误报为前任完成 |
| agent::loop_runner::tests::continuation_tool_identity_matches_conversation_and_native_observers | 1 | conversation/native 两入口的 continuation owner 与会话身份一致 |
| plugins::tool::tests::plugin_model_media_survives_real_process_envelope | 1 | 真实插件进程 envelope 保留 model media 且不伪装 chat delivery |
| plugins::source_runtime::tests::dev_skill_launcher_passes_paths_env_and_arguments_through_the_shell_unchanged | 1 | 真实执行生成的 dev 技能启动脚本：含引号/空格的路径、环境变量与工具参数经 shell 原样到达；清单副本去掉 sha256/signature 且再次同步取到源码清单的修改 |
| tools::coding_tools::tests::exec_cli_receives_the_host_continuation_identity_on_every_command_path | 1 | exec 普通/yield/TTY 每路径都继承宿主 continuation identity |

## 2. 跨功能（多模块协作）

> 功能交叉核问：MCP bearer 解析是否与 `octos-core::constant_time_eq` 组合成真实鉴权边界？由 integration 表中的 `tests/mcp_server.rs::bearer_token_parsing_and_constant_time_comparison`（类别3）回答，覆盖同长度错内容与不同长度，不是常量存在性检查。

| 测试名 | 类别 | 守护性质 |
|---|---|---|
| agent::loop_runner::tests::configured_compaction_cache_tracks_actual_visibility_across_all_compaction_paths | 2 | 压缩缓存跨所有压缩路径跟踪真实可见性 |
| agent::loop_runner::tests::configured_compaction_task_runs_after_large_tool_result_without_provider_overflow | 2 | 大工具结果后压缩任务不导致 provider 溢出 |
| agent::loop_runner::tests::continuation_evidence_chat_uses_stable_session_across_recreated_agents | 2 | 重建 agent 间 continuation evidence 会话稳定 |
| agent::budget::budget_checkpoint_tests::peer_owned_result_survives_budget_checkpoint | 2 | peer 拥有的结果在预算 checkpoint 后存活 |
| agent::budget::budget_checkpoint_tests::no_owner_path_still_writes_result_md | 2 | 无 owner 路径仍写 result.md |
| tools::mcp_agent::tests::read_response_skips_notification_frames | 2 | MCP agent 读取跳过通知帧 |
| mcp::content::tests::should_preserve_images_resources_and_structured_content | 2 | MCP 内容保留图像/资源/结构化内容 |
| mcp::content::tests::should_report_invalid_image_and_preserve_mcp_failure | 2 | 无效图像报告且保留 MCP 失败语义 |
| mcp::tests::statuses_surface_config_oauth_flag_for_both_kinds | 2 | 运行时 statuses() 真实行按 config.oauth 给 true/false（OAuth 连接器可重新授权，普通连接器不误报）— 跨 mcp config→client.statuses 链路 |

## 3. 安全（沙箱/权限/脱敏/SSRF/注入）

### 固定五行边界核问

| 边界 | 准确测试名 | 守护性质 |
|---|---|---|
| 路径穿越 | `tests/security_sandbox.rs::should_reject_path_traversal` | `resolve_path` 对 `..` 与嵌套穿越真实返回错误 |
| 沙箱逃逸 | `tests/security_sandbox.rs::should_reject_absolute_paths` | `resolve_path` 拒绝工作区外绝对路径 |
| 鉴权/权限 | `tests/dangerous_permissions.rs::dangerous_profile_is_gated_to_solo_runtime` | DangerFullAccess 仅 Solo 放行，Local/Tenant/Cloud 均拒收 |
| 密钥/令牌脱敏 | `mcp_auth::tests::should_expose_get_credentials_failure_without_secrets` | get_credentials 失败可见，但 D13 load/access/refresh secret 均不进入状态详情 |
| 外部输入大小上限 | `mcp::tests::validate_schema_rejects_too_deep_and_too_big` | 外部 MCP schema 过深/过大在注册前拒收 |

| 测试名 | 类别 | 守护性质 |
|---|---|---|
| sandbox::tests::confining_backends_never_report_noop_or_refusal | 3 | 隔离后端绝不报告 noop 或拒绝 |
| sandbox::tests::should_degrade_loudly_not_refuse_when_auto_finds_no_backend | 3 | 无后端时响亮降级而非拒绝 |
| sandbox::tests::should_never_refuse_a_default_config_on_any_host | 3 | 默认配置在任何主机上不拒绝 |
| sandbox::tests::refusing_sandbox_command_refuses_and_never_runs_the_original | 3 | 拒绝沙箱命令绝不执行原命令 |
| sandbox::bwrap::tests::test_bwrap_sandbox_env_sanitization | 3 | bwrap 环境变量脱敏 |
| tools::shell::tests::bash_file_writes_deny_refuses_write_command | 3 | bash 文件写 deny 拒绝写命令 |
| tools::shell::tests::shell_safe_policy_still_denies_with_scope_present | 3 | 有 scope 时安全策略仍拒绝 |
| tools::ssrf::tests::test_check_ssrf_blocks_ipv4_mapped_ipv6_private | 3 | SSRF 拦截 IPv4 映射 IPv6 私网 |
| tools::read_file::tests::read_file_refuses_ancestor_symlink_escape | 3 | read_file 拒绝祖先符号链接逃逸 |
| tools::write_file::tests::write_grant_denies_non_allowlisted_path | 3 | write grant 拒绝非白名单路径 |
| tools::write_file::tests::write_grant_create_only_refuses_overwrite | 3 | create-only 拒绝覆盖 |
| tools::write_file::tests::write_grant_ancestor_swap_cannot_capture_external_preview_baseline | 3 | 祖先交换不能捕获外部预览基线 |
| tools::edit_file::tests::write_grant_create_only_refuses_edit_even_on_allowlisted_path | 3 | create-only 即使在白名单也拒绝编辑 |
| tools::edit_file::tests::write_grant_edit_follows_allowlist_without_create_only | 3 | 非 create-only 编辑遵循白名单 |
| plugins::tool::tests::plugin_refuses_input_path_using_symlink_escape_inside_skill_dir | 3 | 插件拒绝技能目录内符号链接逃逸输入 |
| plugins::tool::tests::plugin_refuses_output_path_using_symlink_escape_inside_workspace | 3 | 插件拒绝工作区内符号链接逃逸输出 |
| mcp::tests::protected_names_cover_core_builtins | 3 | MCP 保护名覆盖核心内置工具 |
| mcp_auth::tests::encrypted_fallback_round_trips_without_plaintext_or_cross_profile_reads | 3 | 加密回退往返无明文/跨 profile 读取 |
| plugins::tool::tests::continuation_owner_comes_only_from_host_tool_context | 3 | 插件 continuation owner 只能来自宿主 ToolContext，伪造 extra env 不生效 |
| plugins::tool::tests::plugin_model_media_snapshots_deduplicates_and_rejects_unsafe_paths | 3 | 媒体快照去重且拒绝穿越/绝对/符号链接与后改文件 |
| plugins::install::execution_gate_tests::init_and_shutdown_do_not_execute_without_explicit_allowance | 3 | 远程 lifecycle init/shutdown 未经显式信任门绝不执行，非可执行技能仍可注册工具 |
| plugins::install::execution_gate_tests::no_sandbox_backend_is_rejected_instead_of_silently_running_unconfined | 3 | 无真实沙箱后端时 lifecycle 执行 fail-closed，绝不静默降级为无隔离运行 |
| plugins::install::execution_gate_tests::docker_backend_is_rejected_so_declared_lifecycle_env_is_not_dropped | 3 | Docker 无法将 hardware_lifecycle.env 传入容器，显式拒绝 lifecycle 执行 |
| plugins::install::execution_gate_tests::lifecycle_sandbox_maps_requires_network_to_backend_allow_network | 3 | 生产 lifecycle_sandbox(false/true) 将 requires_network 映射到后端网络策略（bwrap --unshare-net 或 macOS SBPL deny/allow network），Docker-only 主机显式拒绝 |
| plugins::install::execution_gate_tests::allowed_lifecycle_failure_is_the_command_exit_not_sandbox_unavailable | 3 | 允许的 lifecycle 真实执行 exit 7 后报告命令失败而非 sandbox-unavailable 早退，并回滚部分安装 |
| plugins::install::execution_gate_tests::allowed_macos_lifecycle_confines_cwd_and_env_then_fails_real_exit | 3 | macOS 真实 sandbox-exec 后端下 lifecycle 被限制在技能 cwd、env 白名单过滤密钥，越界写真实失败 |
| plugins::source_runtime::tests::only_a_registered_marker_makes_a_dev_skill_and_an_installed_skill_is_kept | 3 | 已安装技能树内自带的 `.octos-dev.json` 无登记即被忽略；`dev` 无 `--force` 不覆盖已安装技能；源码目录消失时明确报错而非加载旧副本 |


## 4. 恢复/数据/并发/协议帧

> 失败/迟到路径核问：`get_credentials` 失败是否既留下可见的持久化失败状态，又不泄露 D13 load/access/refresh secret？由固定类别3中的 `mcp_auth::tests::should_expose_get_credentials_failure_without_secrets` 回答；真实调用 `persist_manager_credentials_with` 与 `refresh_persistence_status`，不是只断言错误字段存在。

| 测试名 | 类别 | 守护性质 |
|---|---|---|
| agent::execution::tests::should_record_cancelled_result_when_turn_cancel_fires | 4 | turn cancel 时记录诚实 cancelled 结果 |
| agent::execution::tests::should_skip_tool_that_starts_after_turn_cancelled | 4 | turn cancel 后启动的工具被跳过 |
| agent::execution::tests::fixed_dispatcher_native_timeout_cancels_mutation_and_drops_future | 4 | 原生超时取消 mutation 并 drop future |
| agent::execution::tests::should_release_execution_lease_when_native_tool_abort_completes | 4 | 原生工具中止完成时释放执行租约 |
| tools::shell::tests::should_terminate_process_group_when_turn_cancelled | 4 | turn cancel 时终止进程组（SIGTERM→SIGKILL） |
| tools::shell::tests::max_timeout_secs_caps_a_larger_requested_timeout | 4 | 超时上限截断更大的请求值 |
| tools::shell::build_cache_usage_tests::cancelled_foreground_retains_build_cache_claim_until_child_exit | 4 | 取消前台保留 build cache claim 至子退出 |
| mcp::wire_tests::turn_cancel_sends_cancelled_notification_for_in_flight_mcp_call | 4 | 正常 baseline 零取消→in-flight cancel 发送对应 request id→late 不改写→同连接恢复且无新增取消 |
| mcp::wire_tests::bounded_line_read_fails_oversized_frame | 4 | 超限帧在读取边界失败 |
| mcp::wire_tests::bounded_line_read_allows_frame_at_cap | 4 | 恰好上限的帧仍有效 |
| mcp::wire_tests::stdio_server_oversized_frame_fails_the_connection | 4 | 真实子进程超限帧使连接失败 |
| mcp::wire_tests::should_send_mcp_screenshot_pixels_to_the_main_model | 4 | MCP 截图像素送达主模型 |
| compaction::tests::test_find_recent_boundary_tool_pairing | 4 | 压缩找到最近边界保持工具配对 |
| compaction::tests::llm_compaction_transcript_preserves_tool_structure_without_hidden_reasoning | 4 | LLM 压缩转录保持工具结构无隐藏推理 |
| agent::loop_compaction::tests::should_preserve_tool_result_validity_after_compaction | 4 | 压缩后工具结果有效性保持 |
| task_supervisor::tests::enable_persistence_reaps_orphan_running_tasks_at_startup | 4 | 启动时持久化回收孤儿运行任务 |
| task_supervisor::tests::on_register_callback_may_reenter_the_supervisor_without_deadlock | 4 | 注册回调可重入 supervisor 无死锁 |
| task_supervisor::tests::cancel_running_task_transitions_to_cancelled_and_fires_token | 4 | 取消运行任务转换终态并触发 token |
| task_supervisor::tests::cancel_token_catches_cancel_between_precheck_and_notify_park | 4 | 取消 token 捕获 precheck 与 park 间取消 |
| task_supervisor::tests::diagnostic_error_preserves_real_owner_failure_and_delayed_ack_recovery | 4 | 诊断错误保留真实 owner 失败与延迟 ack 恢复 |
| cost_ledger::corruption_tests::should_fail_closed_on_malformed_contract_index | 4 | 畸形契约索引 fail-closed |
| cost_ledger::corruption_tests::should_fail_closed_on_malformed_event_rows | 4 | 畸形事件行 fail-closed |
| cost_ledger::corruption_tests::should_fail_closed_on_malformed_session_index | 4 | 畸形会话索引 fail-closed |
| agent::iteration_evidence::tests::large_evidence_within_byte_limit_is_verified_without_a_second_count_limit | 4 | 大 evidence 内全部 artifact binding 仍验证，不能在旧计数截断后漏检 |
| agent::iteration_evidence::tests::large_evidence_does_not_disable_the_existing_record_byte_limit | 4 | 超过既有 record byte limit 仍 fail-closed |
| agent::iteration_evidence::tests::completed_scope_can_expand_and_new_repairs_have_a_comparable_baseline | 4 | 合法扩 scope 保留可比 baseline，不能重置 failed streak 或丢未完成任务 |
| agent::iteration_evidence::tests::delivery_retarget_preserves_the_stagnation_history_and_rejects_other_policy_edits | 4 | delivery retarget 保留停滞历史且拒绝伪造阈值/目标编辑 |
| agent::iteration_evidence::tests::initial_pending_snapshot_is_waiting_not_corrupt_or_a_completed_attempt | 4 | 初始 pending 只表示等待，不能误报完成或损坏 |
| tools::peer_gather::wait_tests::wait_probes_changes_but_only_acknowledges_delivered_results | 4 | wait 探测变化但只 ack 已交付结果，超时仍失败 |
| tools::peer_gather::wait_tests::cancelled_wait_does_not_acknowledge_unseen_results | 4 | 取消等待不 ack 未见结果，后续执行才可交付 |
| tools::peer_publish::tests::only_publishes_bounded_checkpoint_without_caller_chosen_identity | 4 | 只发布有界 checkpoint，拒绝空/超限输入和调用方伪造身份 |

## 5. 有意设计决定

| 测试名 | 类别 | 守护性质 |
|---|---|---|
| agent::append_only_audit::tests::should_report_a_rewrite_when_an_earlier_message_changes_in_place | 5 | 审计：早期消息原地变更必须报告为 rewrite |
| agent::append_only_audit::tests::should_report_a_shrink_when_history_is_dropped | 5 | 审计：历史丢弃必须报告为 shrink |
| agent::append_only_audit::tests::should_report_nothing_when_the_turn_only_appends | 5 | 审计：纯追加不报告（正常路径） |
| agent::append_only_audit::tests::should_report_the_real_in_place_truncation_octos_performs_today | 5 | 审计：报告 octos 实际执行的原地截断 |
| agent::loop_runner::tests::terminal_integrity_mixed_stream_recovery_charges_exactly_once | 5 | 混合终态流恢复时 usage 恰好计费一次 |
| agent::loop_runner::tests::terminal_integrity_mixed_rejected_then_error_retains_usage | 5 | 混合终态拒绝后错误保留 usage |
| agent::memory::tests::episode_injection_preserves_top_match | 5 | 记忆注入保留 top match |
| agent::memory::tests::episode_injection_filters_below_threshold | 5 | 记忆注入过滤低于阈值项 |
| agent::memory::tests::no_embedder_path_does_not_inject_shared_vocab_episode | 5 | 无 embedder 路径不注入共享词汇 episode |

## Integration targets（其余 19 项；固定类别3核问另含 3 项，共 22 项）

| 测试名 | 类别 | 守护性质 |
|---|---|---|
| tests/security_sandbox.rs::should_allow_write_inside_workspace | 3 | macOS sandbox-exec 内核 write/tmp/Python/cross-user/read/network 隔离，非 decision/profile 等价物 |
| tests/security_sandbox.rs::should_block_write_outside_workspace | 3 | macOS sandbox-exec 内核 write/tmp/Python/cross-user/read/network 隔离，非 decision/profile 等价物 |
| tests/security_sandbox.rs::should_block_write_to_etc | 3 | macOS sandbox-exec 内核 write/tmp/Python/cross-user/read/network 隔离，非 decision/profile 等价物 |
| tests/security_sandbox.rs::should_block_write_to_home | 3 | macOS sandbox-exec 内核 write/tmp/Python/cross-user/read/network 隔离，非 decision/profile 等价物 |
| tests/security_sandbox.rs::should_block_tmp_write_without_tmp_allowance | 3 | macOS sandbox-exec 内核 write/tmp/Python/cross-user/read/network 隔离，非 decision/profile 等价物 |
| tests/security_sandbox.rs::should_allow_tmp_write_with_explicit_tmp_allowance | 3 | macOS sandbox-exec 内核 write/tmp/Python/cross-user/read/network 隔离，非 decision/profile 等价物 |
| tests/security_sandbox.rs::should_block_python_write_outside_workspace | 3 | macOS sandbox-exec 内核 write/tmp/Python/cross-user/read/network 隔离，非 decision/profile 等价物 |
| tests/security_sandbox.rs::should_allow_python_write_inside_workspace | 3 | macOS sandbox-exec 内核 write/tmp/Python/cross-user/read/network 隔离，非 decision/profile 等价物 |
| tests/security_sandbox.rs::should_redirect_python_tempfile_via_tmpdir | 3 | macOS sandbox-exec 内核 write/tmp/Python/cross-user/read/network 隔离，非 decision/profile 等价物 |
| tests/security_sandbox.rs::should_isolate_two_users_simultaneously | 3 | macOS 双用户各自可写与 A→B 写入非零/permission stderr，吸收原单独 cross-user 探针 |
| tests/security_sandbox.rs::should_restrict_reads_when_configured | 3 | macOS sandbox-exec 内核 write/tmp/Python/cross-user/read/network 隔离，非 decision/profile 等价物 |
| tests/security_sandbox.rs::should_block_network_when_denied | 3 | macOS sandbox-exec 内核 write/tmp/Python/cross-user/read/network 隔离，非 decision/profile 等价物 |
| tests/read_image_wire.rs::four_tools_deliver_image_pixels_and_retain_them_after_another_read | 1 | 真实 read/agent/provider wire 链保持图像像素与后续读取历史一致（main 后续新增补录） |
| tests/mcp_server.rs::bearer_token_parsing_and_constant_time_comparison | 3 | Bearer 大小写/空白解析与 octos-core 常数时间比较，拒绝同长度错值和不同长度 |
| tests/manage_skills_hash.rs::bad_signature_is_an_error_not_a_source_build_fallback | 3 | 错误 Ed25519 signer 拒收且不 fallback，main 不落地 |
| tests/manage_skills_hash.rs::substituted_bytes_with_original_proof_are_rejected | 3 | 原始 proof 下的替换字节因 SHA-256 不符拒收，main 不落地 |
| tests/spawn_only_interrupt_cancellation.rs::supervisor_cancel_aborts_running_spawn_only_task | 4 | detached future 真 drop 与 router.mark_terminal 真实取消收尾 |
| tests/spawn_only_interrupt_cancellation.rs::cancel_runs_terminal_teardown_for_routed_spawn_only_task | 4 | detached future 真 drop 与 router.mark_terminal 真实取消收尾 |
| tests/validator_runner.rs::windows_command_validator_fails_closed_when_appcontainer_helper_vanished | 4 | Windows AppContainer helper 缺失时 fail-closed，marker 证明不 raw host spawn |
