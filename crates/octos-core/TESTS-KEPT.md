# octos-core TESTS-KEPT

本清单是 T-0521 删除工作的**保留白名单**。基线为 octos main `aefbe077`：`wake/test-lines.py --json` 实测 **4,675 行 / 201 个测试**；名称来自 `cargo test -p octos-core --all-targets -- --list`。

类别：**1 跨系统，2 跨功能，3 安全，4 恢复/数据/并发/协议帧，5 有意设计**。只保留能写出具体测试级守护性质的项；普通 serde/默认值、重复小切片和“文件可能有用”不留。第 5 类必须对应明确的设计契约。

## OUP / typed errors / bounded frames

| 准确 libtest 名 | 类别 | 保留守护性质 |
|---|---:|---|
| `app_ui_codec::tests::error_golden_roundtrip` | 4 | OUP 错误帧 golden 编解码不漂移。 |
| `app_ui_codec::tests::malformed_frames_are_rejected_deterministically` | 4 | 畸形协议帧确定性失败而非进入不确定状态。 |
| `app_ui_codec::tests::notification_golden_roundtrip` | 4 | notification 帧字段与 round-trip 保持协议契约。 |
| `app_ui_codec::tests::oversized_frame_is_rejected_before_deserialization` | 4 | 超大帧在反序列化前拒绝，防止内存放大型输入。 |
| `app_ui_codec::tests::request_golden_roundtrip` | 4 | request 帧 golden 编解码不漂移。 |
| `app_ui_codec::tests::response_golden_roundtrip` | 4 | response 帧 golden 编解码不漂移。 |
| `bounded_read::tests::read_to_end_capped_accepts_in_cap_content` | 4 | 有界读取在 cap 内完整保留数据且不误报溢出。 |
| `bounded_read::tests::read_to_end_capped_rejects_overflow` | 4 | 超过 cap 的输入响亮失败，不无界分配。 |
| `error::tests::test_error_display_truncates_long_body` | 3 | typed error 展示截断超长远端 body，限制敏感信息/日志放大。 |
| `error::tests::test_source_returns_io_error` | 4 | typed error 保留底层 I/O source 链供恢复诊断。 |
| `ui_protocol::tests::rpc_error_code_names_and_values_are_unique` | 4 | OUP 错误码名称和值保持唯一，避免 wire 冲突。 |
| `ui_protocol::tests::spec_section_10_carries_no_numeric_code_outside_rpc_error_codes` | 4 | §10 孤立数字码 orphan 扫描阻止 spec 与 Rust 单侧漂移。 |
| `ui_protocol::tests::spec_section_10_pins_every_rpc_error_code` | 4 | §10 双向 pin 住每个 Rust 数字错误码及 spec 映射。 |
| `ui_protocol::tests::spec_v2_projection_examples_accept_windows_line_endings` | 4 | §14 示例解析接受 CRLF，跨平台契约不被换行差异破坏。 |
| `ui_protocol::tests::spec_v2_projection_examples_match_rust_wire_types` | 4 | §14 可执行 projection 示例与 Rust wire 类型双向一致。 |
| `ui_protocol::tests::unpinned_numeric_code_scan_flags_unknown_and_accepts_pinned_codes` | 4 | §10 扫描器自身的未知码正例与 pinned 码负例都可执行验证。 |

## Autonomy typed method registry (T-0118 A-08, UPCR-2026-RA2A9967B)

| 准确 libtest 名 | 类别 | 保留守护性质 |
|---|---:|---|
| `autonomy_rpc::tests::should_cover_every_autonomy_method_once_with_its_feature_group` | 4 | 24 个 autonomy 方法逐一在册且 feature 分组不漂移（capability 与 dispatch 同源）。 |
| `autonomy_rpc::tests::should_reject_unknown_methods_as_method_not_found` | 4 | 非 autonomy 方法保持 method_not_found，未被 registry 吞并。 |
| `autonomy_rpc::tests::should_decode_published_wire_shapes_onto_the_typed_variants` | 4 | 已发布 param JSON 逐字段落到 typed variant；未知字段保持忽略（无 deny_unknown_fields 的 wire 契约）。 |
| `autonomy_rpc::tests::should_default_optional_list_params_from_null_and_empty_objects` | 4 | 三个 list 方法 null/{} 均取默认；必填方法仍拒绝 null params。 |
| `autonomy_rpc::tests::should_accept_loop_create_command_and_input_aliases` | 4 | `loop/create` 的 `command`/`input` 双别名等价，双写仍报重复字段。 |
| `autonomy_rpc::tests::should_report_invalid_params_with_the_published_method_prefix` | 4 | 缺必填字段保持 `{method} params:` 前缀的既有错误格式。 |
| `autonomy_rpc::tests::should_scope_task_artifact_aliases_to_an_explicit_session` | 3 | `task_id` 无 `session_id` 失败关闭（#1121），`agent_id` 优先，两者皆缺报原文错误。 |

## Credential / process-environment hygiene

| 准确 libtest 名 | 类别 | 保留守护性质 |
|---|---:|---|
| `env_hygiene::tests::git_config_groups_survive_whole_or_are_removed_whole` | 4 | 原子清理 git config group，避免部分删除改变配置语义。 |
| `env_hygiene::tests::real_git_reads_sanitized_config_groups_without_touching_global_config` | 1 | 真实 git 子进程读取清理后 config 且不修改全局配置。 |
| `env_hygiene::tests::registered_provider_secret_is_flagged_even_when_not_heuristic` | 3 | 已注册 provider secret 即使名称不启发式匹配也必须识别。 |
| `env_hygiene::tests::sanitize_git_command_env_strips_injection_vars` | 3 | git 命令环境中的注入变量在执行前移除。 |

## Upload / tenant / file-handle containment

| 准确 libtest 名 | 类别 | 保留守护性质 |
|---|---:|---|
| `file_handle::tests::is_under_upload_root_distinguishes_upload_from_other_paths` | 3 | upload root 与其他路径严格区分，防止 handle 越界。 |
| `file_handle::tests::legacy_absolute_request_is_scoped` | 3 | 旧绝对路径请求也被限定到对应 scope，而非宿主任意读。 |
| `file_handle::tests::materialize_drops_foreign_image` | 3 | 非本租户图片不得进入工作区 vision 路径。 |
| `file_handle::tests::materialize_matching_tenant_materializes_mismatch_and_flat_dropped` | 3 | 同 tenant 可物化，mismatch 与扁平路径均拒绝。 |
| `file_handle::tests::materialize_never_overwrites_on_name_collision` | 4 | upload 文件名冲突不覆盖既有数据。 |
| `file_handle::tests::materialize_refuses_symlinked_uploads_dir` | 3 | uploads 目录为 symlink 时拒绝物化，防止目录逃逸。 |
| `file_handle::tests::materialize_rejects_raw_tmpdir_path_bypass` | 3 | 原始 tmpdir 路径不能绕过 upload handle 权限链。 |
| `file_handle::tests::upload_owned_by_tenant_keys_off_first_component_under_root` | 3 | tenant 所有权从 root 下首组件判定，不能伪造归属。 |
| `file_handle::tests::workspace_handle_round_trips_only_within_its_workspace` | 3 | workspace handle 只在本 workspace 内往返，跨根拒绝。 |

## Git worktree hooks / recovery / lock ownership

| 准确 libtest 名 | 类别 | 保留守护性质 |
|---|---:|---|
| `agent_repo_git::tests::should_not_run_local_config_programs_when_kernel_runs_git_in_agent_repo` | 3 | kernel 在 agent repo 调 git 时不执行仓库本地 config program。 |
| `git_worktree::tests::controller_git_ops_do_not_fire_repo_hooks` | 3 | controller 内部 git 操作不得执行仓库 hook。 |
| `git_worktree::tests::deliverable_commit_command_escapes_the_message` | 3 | deliverable commit message 正确转义，防止命令注入。 |
| `git_worktree::tests::deliverable_commit_command_lands_uncommitted_changes` | 1 | 真实 git 链能把未提交产出纳入 deliverable commit。 |
| `git_worktree::tests::global_filter_is_neutralized_on_controller_ops` | 3 | 全局 filter/驱动配置不能污染 controller worktree 操作。 |
| `git_worktree::tests::prepare_reclaims_a_locked_leftover_worktree` | 4 | 遗留 locked worktree 可被恢复复用而非永久卡住。 |
| `git_worktree::tests::prepare_reconciles_existing_branch_and_checkout_resuming_from_branch` | 4 | 已存在 branch/checkout 能从 branch 状态一致恢复。 |
| `git_worktree::tests::prepare_rejects_symlinked_checkout` | 3 | checkout 为 symlink 时拒绝复用，防止写向任意外部目录。 |
| `git_worktree::tests::prepare_rejects_symlinked_parent` | 3 | checkout 父目录为 symlink 时拒绝，防止祖先逃逸。 |
| `git_worktree::tests::removing_one_checkout_must_not_unregister_a_sibling_worktree` | 4 | 删除一个 checkout 不得误删 sibling 的注册/锁所有权。 |
| `git_worktree::tests::validate_fleet_branch_rejects_injection_and_traversal` | 3 | fleet branch 名拒绝注入与路径穿越。 |

## Secret redaction

| 准确 libtest 名 | 类别 | 保留守护性质 |
|---|---:|---|
| `secret_redaction::tests::should_follow_documented_boundaries_when_inputs_are_ambiguous` | 5 | 固定 [模块文档](src/secret_redaction.rs)定义的歧义输入边界，防止安全语义被无意改写。 |
| `secret_redaction::tests::should_keep_auth_scheme_when_credential_value_starts_with_scheme` | 3 | redaction 不误删认证 scheme 或把 value 中 scheme 当作新凭据。 |
| `secret_redaction::tests::should_keep_env_references_and_empty_strings_when_key_is_credential_named` | 3 | credential key 下的 env 引用与空值不被误当真实 secret。 |
| `secret_redaction::tests::should_keep_original_formatting_when_embedded_json_has_no_secrets` | 3 | 无 secret 的嵌入 JSON 保持原文格式，避免过度脱敏破坏数据。 |
| `secret_redaction::tests::should_keep_symbolic_reference_when_key_is_credential_named` | 3 | credential key 下的符号引用保持可用而不误脱敏。 |
| `secret_redaction::tests::should_leave_output_unchanged_when_redacting_already_redacted_output` | 4 | 对已脱敏输出再次处理保持幂等。 |
| `secret_redaction::tests::should_not_panic_when_input_is_odd_or_adversarial` | 3 | 对抗性/异常形状输入不 panic，fail-safe 返回。 |
| `secret_redaction::tests::should_redact_basic_auth_after_curl_user_flag` | 3 | curl `-u` 后的 basic auth 凭据必须脱敏。 |
| `secret_redaction::tests::should_redact_bearer_and_basic_credentials_when_authorization_header_in_text` | 3 | 文本中的 bearer/basic Authorization 值均脱敏。 |
| `secret_redaction::tests::should_redact_numeric_credentials_in_embedded_json_and_argv` | 3 | 嵌入 JSON 与 argv 中的数值型凭据也不能漏出。 |
| `secret_redaction::tests::should_redact_structurally_when_string_value_is_embedded_json` | 3 | 字符串中的嵌入 JSON 按结构脱敏而非只做表层正则。 |
| `secret_redaction::tests::should_redact_value_following_secret_flag_when_argv_array_splits_flag_and_value` | 3 | argv 将 secret flag 与值拆开时仍能识别并脱敏。 |
| `secret_redaction::tests::should_redact_whole_token_when_text_contains_recognizable_api_key` | 3 | 可识别 API key 在任意文本中完整脱敏。 |
| `secret_redaction::tests::should_treat_containers_as_opaque_text_when_nesting_exceeds_depth_cap` | 3 | 超过嵌套深度 cap 后按不透明文本处理，避免递归 DoS。 |

## Session / tenant / shared-zone containment

| 准确 libtest 名 | 类别 | 保留守护性质 |
|---|---:|---|
| `session_scope::tests::another_sessions_workspace_classifies_as_out_of_scope` | 3 | 其他 session 的 workspace 永远越界。 |
| `session_scope::tests::canonical_root_and_candidate_match_for_missing_root` | 4 | root 与候选均不存在时仍能一致分类，不因存在性竞态误判。 |
| `session_scope::tests::canonical_root_and_candidate_match_for_missing_root_behind_symlink` | 4 | 缺失 root 位于 symlink 后仍保持同一规范化语义。 |
| `session_scope::tests::classify_canonical_path_refuses_symlink_escape_from_skill_dir` | 3 | skill read zone 内经 symlink 逃逸到外部必须拒绝。 |
| `session_scope::tests::classify_path_out_of_scope_for_other_tenant` | 3 | 其他 tenant 的路径不得进入当前 scope。 |
| `session_scope::tests::classify_path_refuses_parent_dir_components` | 3 | `..` 组件不能绕过 workspace/zone 边界。 |
| `session_scope::tests::multi_tenant_at_workspace_accepts_encoded_under_data_layout` | 2 | multi-tenant data layout 的编码路径能映射到正确 workspace。 |
| `session_scope::tests::multi_tenant_layout_matches_handlers_rs_today` | 2 | scope 计算与真实 handlers 的多租户目录布局保持一致。 |
| `session_scope::tests::refuses_shared_zone_outside_root` | 3 | shared zone 不得指向 root 外部。 |
| `session_scope::tests::refuses_shared_zone_overlapping_users_subtree` | 3 | shared zone 不得覆盖 users 私有子树。 |
| `session_scope::tests::refuses_unsafe_session_id` | 3 | 不安全 session id 在路径构造前拒绝。 |
| `session_scope::tests::schema_version_bumped_to_v2_for_skill_read_zones` | 4 | 增加 skill read zones 的持久 scope schema 必须显式升级 v2。 |
| `session_scope::tests::should_classify_the_peer_folder_as_read_only_view_when_the_scope_has_one` | 3 | peer folder 只能进入只读 view，不能获得写权限。 |
| `session_scope::tests::should_refuse_a_symlink_into_another_context_when_it_sits_in_the_view` | 3 | view 内指向其他 context 的 symlink 必须拒绝。 |
| `session_scope::tests::solo_granted_dir_outranks_skill_read_zone` | 2 | solo 显式授权目录与 skill zone 重叠时优先级稳定。 |
| `session_scope::tests::with_workspace_rejects_symlinked_workspace_escape` | 3 | workspace 自身经 symlink 指向外部时不得通过 scope。 |

## JSONL / schema compatibility and recovery

| 准确 libtest 名 | 类别 | 保留守护性质 |
|---|---:|---|
| `task::tests::should_default_missing_schema_version_to_v1` | 4 | 缺失 task schema 版本按 v1 恢复旧数据。 |
| `task::tests::should_default_missing_task_result_schema_version_to_v1` | 4 | 缺失 task-result schema 版本按 v1 恢复旧结果。 |
| `task::tests::should_reject_future_schema_version_with_actionable_error` | 4 | 未来 schema 版本明确拒绝并给出可行动错误。 |
| `task::tests::should_round_trip_session_summary_byte_identical` | 4 | session summary 持久化 round-trip 字节不变。 |
| `types::tests::fork_child_preserves_profile_and_channel` | 4 | fork 子 session 保留 profile/channel 身份，避免恢复后串道。 |
| `types::tests::message_deserializes_legacy_jsonl_without_thread_id` | 4 | 旧 JSONL 缺少 thread_id 时仍可恢复。 |
| `types::tests::message_source_roundtrips_and_legacy_rows_omit_it` | 4 | message source 新旧 JSONL 往返兼容且旧行字段缺省。 |
| `types::tests::should_round_trip_participant_and_omit_it_when_absent` | 4 | participant 新字段 round-trip 且旧行缺省兼容。 |
| `types::tests::typed_message_round_trips_through_jsonl` | 4 | typed message 经真实 JSONL 编码/解码保持数据完整。 |

## UTF-8 / byte budgets / safe filenames / comparison

| 准确 libtest 名 | 类别 | 保留守护性质 |
|---|---:|---|
| `utils::tests::should_clamp_with_distinct_hash_suffix_when_names_share_long_prefix` | 4 | 长同前缀文件名截断后仍靠 hash 保持可区分。 |
| `utils::tests::should_contain_no_path_or_dot_bytes_when_input_is_hostile` | 3 |  hostile 输入生成的安全文件名不含路径或 dot 穿越字节。 |
| `utils::tests::should_emit_exact_legacy_marker_when_truncating` | 4 | 截断 marker 与 legacy 字节契约完全一致。 |
| `utils::tests::should_emit_only_marker_when_budget_below_marker_overhead` | 4 | 预算小于 marker 开销时仍不越界、不 panic。 |
| `utils::tests::should_keep_utf8_boundaries_when_multibyte_chars_straddle_the_cut` | 4 | 多字节 UTF-8 字符跨切割点时保持合法边界。 |
| `utils::tests::should_not_split_percent_triplet_when_clamping` | 4 | 文件名 percent triplet 不被截成非法序列。 |
| `utils::tests::should_percent_encode_specials_and_cjk_when_safe_filename` | 4 | 特殊字符/CJK 经 percent 编码后形成安全可恢复文件名。 |
| `utils::tests::should_report_bytes_truncation_when_one_byte_over_limit` | 4 | 超 cap 一字节也准确报告截断。 |
| `utils::tests::test_constant_time_eq` | 3 | 安全比较保持 constant-time 与正确相等语义。 |

共 **95 项**。其它测试只有在补写出具体类别与测试级守护性质后才可加入，不得复活旧小切片中已删除的结构镜像。
