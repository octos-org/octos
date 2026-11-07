# octos-agent TESTS-KEPT

保留清单（T-0506 / #3069）：按守护性质分五类。类别：1=跨系统（Agent→工具→结果链路）、2=跨功能（多模块协作）、3=安全（沙箱/权限/脱敏/SSRF/注入）、4=恢复/数据/并发/协议帧、5=有意设计决定。

实测基线：`cargo +1.97.1 test -p octos-agent --lib -- --list` 在当前 main（ed914b5fc）列出 3218 个测试；下列每条均经该列表核名存在。

## 1. 跨系统（Agent→工具→结果）

| 测试名 | 类别 | 守护性质 |
|---|---|---|
| agent::loop_runner::tests::process_message_preserves_tool_pair_order_across_iterations | 1 | 跨迭代工具调用/结果配对不错乱 |
| agent::loop_runner::tests::inject_synthetic_results_pushes_assistant_then_tool_for_every_call | 1 | 合成结果注入保持 assistant→tool 配对 |
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

## 2. 跨功能（多模块协作）

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

## 3. 安全（沙箱/权限/脱敏/SSRF/注入）

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
| mcp::tests::validate_schema_rejects_too_deep_and_too_big | 3 | MCP schema 拒绝过深/过大 |
| mcp::tests::protected_names_cover_core_builtins | 3 | MCP 保护名覆盖核心内置工具 |
| mcp_auth::tests::encrypted_fallback_round_trips_without_plaintext_or_cross_profile_reads | 3 | 加密回退往返无明文/跨 profile 读取 |

## 4. 恢复/数据/并发/协议帧

| 测试名 | 类别 | 守护性质 |
|---|---|---|
| agent::execution::tests::should_record_cancelled_result_when_turn_cancel_fires | 4 | turn cancel 时记录诚实 cancelled 结果 |
| agent::execution::tests::should_skip_tool_that_starts_after_turn_cancelled | 4 | turn cancel 后启动的工具被跳过 |
| agent::execution::tests::fixed_dispatcher_native_timeout_cancels_mutation_and_drops_future | 4 | 原生超时取消 mutation 并 drop future |
| agent::execution::tests::should_release_execution_lease_when_native_tool_abort_completes | 4 | 原生工具中止完成时释放执行租约 |
| tools::shell::tests::should_terminate_process_group_when_turn_cancelled | 4 | turn cancel 时终止进程组（SIGTERM→SIGKILL） |
| tools::shell::tests::max_timeout_secs_caps_a_larger_requested_timeout | 4 | 超时上限截断更大的请求值 |
| tools::shell::build_cache_usage_tests::cancelled_foreground_retains_build_cache_claim_until_child_exit | 4 | 取消前台保留 build cache claim 至子退出 |
| mcp::wire_tests::turn_cancel_sends_cancelled_notification_for_in_flight_mcp_call | 4 | turn cancel 发送对应 request id 的取消通知 |
| mcp::wire_tests::normal_mcp_call_sends_no_cancellation | 4 | 正常调用不发送取消通知 |
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

## 5. 有意设计决定

| 测试名 | 类别 | 守护性质 |
|---|---|---|
| agent::append_only_audit::tests::should_report_a_rewrite_when_an_earlier_message_changes_in_place | 5 | 审计：早期消息原地变更必须报告为 rewrite |
| agent::append_only_audit::tests::should_report_a_shrink_when_history_is_dropped | 5 | 审计：历史丢弃必须报告为 shrink |
| agent::append_only_audit::tests::should_report_nothing_when_the_turn_only_appends | 5 | 审计：纯追加不报告（正常路径） |
| agent::append_only_audit::tests::should_report_the_real_in_place_truncation_octos_performs_today | 5 | 审计：报告 octos 实际执行的原地截断 |
| agent::loop_runner::tests::inject_synthetic_results_sanitizes_tool_call_ids_with_colons | 5 | 合成结果注入时冒号 tool_call_id 消毒 |
| agent::loop_runner::tests::terminal_integrity_mixed_stream_recovery_charges_exactly_once | 5 | 混合终态流恢复时 usage 恰好计费一次 |
| agent::loop_runner::tests::terminal_integrity_mixed_rejected_then_error_retains_usage | 5 | 混合终态拒绝后错误保留 usage |
| agent::memory::tests::episode_injection_preserves_top_match | 5 | 记忆注入保留 top match |
| agent::memory::tests::episode_injection_filters_below_threshold | 5 | 记忆注入过滤低于阈值项 |
| agent::memory::tests::no_embedder_path_does_not_inject_shared_vocab_episode | 5 | 无 embedder 路径不注入共享词汇 episode |
