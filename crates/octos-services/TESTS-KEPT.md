# octos-services TESTS-KEPT

本清单是 T-0517 删除工作的**保留白名单**，不是“可能有帮助”的文件级罗列。基线为 octos main `5377d32a`：`wake/test-lines.py --json` 实测本 crate **4,989 行 / 227 个测试**；名称来自：

```bash
cargo test -p octos-services --all-targets -- --list
```

只保留能明确归入以下性质的测试：**1 跨系统，2 跨功能，3 安全，4 恢复/数据/并发/协议帧，5 有意设计**。本清单不需要第 5 类；serde/default/字段镜像、普通 CRUD 和已被真实链路覆盖的小切片不留。`#[ignore]` 的真实系统测试保留，因为它们是手动运行真实后端时的唯一跨系统证据。

## auth / keychain / secret store

| 准确 libtest 名 | 类别 | 保留守护性质 |
|---|---:|---|
| `auth::keychain::tests::should_reject_read_through_account_symlink` | 3 | 拒绝经账号 symlink 读取任意外部 secret。 |
| `auth::keychain::tests::should_replace_account_symlink_without_touching_target` | 3 | 覆盖账号时替换链接本身，绝不写 symlink target。 |
| `auth::keychain::tests::should_refuse_symlinked_secret_directory_without_chmod_or_write` | 3 | secret 目录为链接时读写删全部失败且不 chmod/创建外部内容。 |
| `auth::keychain::tests::should_replace_hardlinked_account_without_changing_other_link` | 3 | hardlink 账号更新不得改写另一目录项的 secret。 |
| `auth::keychain::tests::should_leave_no_temporary_secret_when_atomic_replace_fails` | 4 | 原子替换失败不遗留临时明文文件。 |
| `auth::keychain::tests::should_reject_fifo_account_without_blocking` | 3 | FIFO 等非普通账号文件 fail-closed，避免读取阻塞。 |
| `auth::keychain::tests::should_not_create_missing_store_during_read_or_delete` | 4 | 缺失 store 的读/删保持无副作用且返回正常未配置态。 |
| `auth::keychain::tests::should_keep_owner_read_write_permissions_under_restrictive_umask` | 3 | 极端 umask 下 secret 文件仍强制 0600。 |
| `auth::keychain::tests::should_refuse_symlinked_store_parent` | 3 | store 任一父目录为 symlink 时不向外部写删 secret。 |
| `auth::keychain::tests::linux_file_backend_roundtrip_with_permissions` | 1 | Linux 文件 backend 的真实 CRUD、profile 隔离、多行 secret 与 0700/0600 端到端链路。 |
| `auth::keychain::tests::linux_file_backend_rejects_path_escaping_names` | 3 | 账号名的绝对/相对路径逃逸均在写盘前拒绝。 |
| `auth::keychain::tests::needs_relocation_closes_custom_env_name_bypass` | 3 | 自定义 env 名不能绕过 service-account 凭据迁移到 keychain。 |
| `auth::keychain::tests::keychain_integration_scoped_accounts_dont_collide` | 1 | 真实 macOS Keychain 上两个 profile 的写删互不影响（手动 ignored）。 |
| `auth::keychain::tests::keychain_integration_roundtrip` | 1 | 真实 macOS Keychain 的 set/get/marker/delete 全生命周期（手动 ignored）。 |
| `auth::store::tests::at_tightens_loose_permissions_on_open` | 3 | 打开既有宽松权限 auth store 时主动收紧权限。 |
| `auth::store::tests::save_writes_0600` | 3 | 保存认证材料始终使用 0600。 |
| `auth::store::tests::should_keep_not_found_as_missing_for_directory_or_absent_auth_home` | 4 | auth home 为目录或不存在时仍正确区分未配置与真实故障。 |
| `auth::store::tests::should_not_treat_windows_not_found_under_file_auth_home_as_missing_store` | 4 | Windows 上 auth home 为文件时不得把路径错误吞成“未配置”。 |
| `auth::store::tests::test_persistence` | 1 | 认证 store 写入后重新打开仍能恢复真实凭据。 |

## CLI child process boundary

| 准确 libtest 名 | 类别 | 保留守护性质 |
|---|---:|---|
| `cli_agent_adapter::tests::argv_config_does_not_invoke_shell_metacharacters` | 3 | 配置参数直接成为 argv，shell 元字符不能变成命令注入。 |
| `cli_agent_adapter::tests::blocked_env_vars_do_not_reach_child_via_config_env` | 3 | 配置显式 env 也不能向子进程注入被禁变量。 |
| `cli_agent_adapter::tests::blocked_parent_env_vars_are_scrubbed_but_inheritance_survives` | 3 | 父进程危险变量被清理且正常环境继承不被误删。 |
| `cli_agent_adapter::tests::cancel_reports_cancelled_and_stops_process` | 4 | 取消可终止真实子进程并返回一致取消状态。 |
| `cli_agent_adapter::tests::times_out_and_kills_child` | 4 | 超时 kill/reap 真实子进程，防止后台泄漏。 |
| `cli_agent_adapter::tests::transcript_capture_is_bounded_and_marks_truncation` | 4 | 子进程输出有界且截断显式可见。 |
| `cli_agent_adapter::tests::relative_declared_artifacts_resolve_against_child_cwd` | 2 | artifact 声明与真实子进程工作目录保持同一解析基准。 |

## config / profile / credential resolution

| 准确 libtest 名 | 类别 | 保留守护性质 |
|---|---:|---|
| `config::tests::custom_api_key_env_override_stays_exclusive` | 3 | 自定义 key env 不得叠加继承另一个提供者的凭据。 |
| `config::tests::default_name_override_ignores_top_level_api_key_env` | 3 | 默认模型覆盖不能意外复用顶层 key，造成跨 provider 凭据串用。 |
| `config::tests::explicit_env_var_skips_provider_auth_store` | 2 | 显式环境凭据与 auth-store 回退的优先级跨配置/认证链路一致。 |
| `config::tests::get_api_key_reads_global_xdg_auth_store` | 1 | API key 能从真实 XDG 全局认证 store 跨模块恢复。 |
| `config::tests::get_api_key_with_env_resolves_through_config_env_vars` | 2 | 配置中的 env 映射能贯穿到最终 key 解析。 |
| `config::tests::load_default_prefers_xdg_config_home_over_legacy` | 1 | 默认加载在 XDG 与 legacy 同时存在时选择正确真实配置链。 |
| `config::tests::load_default_uses_defaults_when_no_config_anywhere` | 4 | 完全未配置时进入安全默认态而不是错误或误读其他目录。 |
| `config::tests::load_explicit_ignores_project_local_config` | 3 | 显式配置模式不得被不可信项目本地配置替换。 |
| `config::tests::load_explicit_never_reads_host_legacy_octos` | 3 | 显式隔离环境不得回读宿主 legacy 凭据/配置。 |
| `config::tests::research_lane_changed_endpoint_does_not_inherit_a_custom_chat_key` | 3 | research 改 endpoint 后不能继承 chat 的私有 key。 |
| `config::tests::research_lane_inherits_matching_chat_fallback_credentials_and_route` | 2 | 同 provider fallback 的凭据与路由能一致传递到 research lane。 |
| `config::tests::research_lane_never_borrows_an_unrelated_primary_key` | 3 | research lane 不得借用不相关主 provider 的凭据。 |
| `config::tests::should_keep_missing_store_as_ordinary_unset_key` | 4 | 缺失认证 store 是普通未设置 key，而非降级故障。 |
| `config::tests::should_never_serialize_cloud_token` | 3 | cloud token 不得进入序列化配置或持久化输出。 |
| `config::tests::should_not_resolve_cloud_token_when_no_cloud_config` | 4 | 未配置 cloud 时不解析/泄漏其他环境 token。 |
| `config::tests::should_prefer_valid_store_without_degraded_fallback` | 4 | 有效 store 不被环境 fallback 覆盖，也不误报 degraded。 |
| `config::tests::should_preserve_metadata_access_error_when_auth_home_is_not_directory` | 4 | auth home 类型错误的真实 metadata 故障不得吞成未配置。 |
| `config::tests::should_preserve_non_utf8_read_error_with_path` | 4 | 非 UTF-8 认证文件保留带路径的可诊断错误。 |
| `config::tests::should_preserve_parse_error_chain_for_truncated_and_wrong_schema_store` | 4 | 截断/错误 schema 的认证文件保留解析错误链，禁止静默降级。 |
| `config::tests::should_preserve_real_read_error_with_path` | 4 | 真实读取失败保留路径与原因，不伪装成 key 不存在。 |
| `config::tests::should_redact_cloud_token_in_debug` | 3 | Debug/日志投影必须 redact cloud token。 |
| `config::tests::should_warn_auth_store_degraded_when_env_fallback_succeeds` | 4 | 认证 store 损坏但 env 可回退时仍明确发出降级警告。 |
| `config::tests::should_reject_approval_policy_when_rule_invalid` | 3 | 无审批人/零过期等不可应答审批配置必须加载失败而非失控。 |
| `config::tests::write_mutation_preserves_unknown_fields` | 4 | 配置写回不得丢弃新版本或宿主拥有的未知字段。 |

## config migration / filesystem recovery

| 准确 libtest 名 | 类别 | 保留守护性质 |
|---|---:|---|
| `config_context::tests::atomic_copy_into_applies_0600_mode` | 3 | 迁移/复制认证材料时目标始终 0600。 |
| `config_context::tests::atomic_copy_into_is_idempotent_and_nondestructive` | 4 | 重复原子复制不破坏既有目标且结果幂等。 |
| `config_context::tests::auth_migration_does_not_touch_tenant_config_dir` | 3 | 全局认证迁移不得越界修改 tenant 配置目录。 |
| `config_context::tests::auth_migration_produces_0600_and_leaves_legacy_intact` | 4 | 认证迁移保留 legacy 可回滚副本并生成 0600 新文件。 |
| `config_context::tests::cli_data_dir_isolates_config_but_auth_stays_global` | 2 | CLI 数据目录隔离配置但认证仍按设计共享，边界不混淆。 |
| `config_context::tests::config_migration_copies_legacy_to_xdg_and_leaves_legacy_intact` | 4 | 配置迁移到 XDG 后保留 legacy 数据用于恢复。 |

## model download / provider construction

| 准确 libtest 名 | 类别 | 保留守护性质 |
|---|---:|---|
| `embed_model::tests::should_error_when_the_transfer_stalls_mid_body` | 4 | 下载 body 中途停滞必须有界失败，不能永久等待。 |
| `embed_model::tests::should_never_run_two_fetches_at_once_when_callers_race` | 4 | 并发调用共享同一模型获取，避免重复下载与写冲突。 |
| `embed_model::tests::should_refuse_when_absent_and_download_disabled` | 3 | 模型缺失且禁用下载时 fail-closed，不擅自联网。 |
| `embed_model::tests::should_report_absent_incomplete_and_complete_models` | 4 | 缺失/不完整/完整三态可区分，残缺模型不会当成可用。 |
| `embed_model::tests::should_retry_transient_fetch_failures_and_recover` | 4 | 短暂下载失败可在预算内重试并恢复。 |
| `embed_model::tests::should_split_one_shared_budget_across_retry_attempts` | 4 | 所有重试共享一个总时间预算，不能逐次重置超时。 |
| `embed_model::tests::should_stream_hash_and_rename_a_matching_download` | 4 | 下载边写边校验，hash 匹配后才原子替换为可用模型。 |
| `provider_factory::custom_provider_tests::custom_openai_provider_discovers_window_before_compaction` | 2 | 自定义 provider 的真实 window 发现先于 compaction 生效。 |
| `provider_factory::custom_provider_tests::rejects_custom_provider_without_base_url` | 3 | 缺少 base URL 的自定义 provider 不得误连默认/错误服务。 |

## session data / tenant isolation

| 准确 libtest 名 | 类别 | 保留守护性质 |
|---|---:|---|
| `soul_service::tests::should_isolate_soul_by_session_base_key` | 2 | 不同 session base key 的 soul 数据不得串联。 |
| `soul_service::tests::should_return_none_when_no_soul_file` | 4 | soul 未配置文件是正常空态而非异常。 |
| `soul_service::tests::should_share_soul_across_topics_for_same_chat` | 2 | 同一 chat 的 topics 按设计共享 soul，session/topic 键不漂移。 |
| `tenant::tests::should_allocate_next_ssh_port` | 4 | tenant 分配不会复用已占用 SSH 端口。 |
| `tenant::tests::should_reject_path_traversal_ids` | 3 | tenant id 路径穿越在进入文件操作前拒绝。 |

## updater integrity and recovery

| 准确 libtest 名 | 类别 | 保留守护性质 |
|---|---:|---|
| `updater::tests::bundle_whitelist_matches_bundle_release_script` | 2 | updater 安装白名单与实际 release bundle 脚本保持一致。 |
| `updater::tests::replace_binaries_installs_only_whitelisted_entries` | 3 | 替换二进制时 archive 非白名单条目绝不落盘。 |
| `updater::tests::update_cleans_the_tmp_dir_when_the_download_fails` | 4 | 下载失败后清理临时目录，不留下可误用的半成品。 |
| `updater::tests::update_refuses_an_archive_with_no_whitelisted_entries` | 3 | 没有任何白名单条目的更新包必须拒绝安装。 |
| `updater::tests::update_refuses_checksum_mismatch_before_installing` | 3 | checksum 不匹配在安装前拒绝，防止篡改包落地。 |
| `updater::tests::update_refuses_sidecar_that_does_not_cover_the_asset` | 3 | sidecar 未覆盖目标 asset 时不得跳过完整性校验。 |
| `updater::tests::update_refuses_size_mismatch_before_installing` | 3 | 下载大小不符在安装前拒绝，截断包不能进入替换流程。 |
| `updater::tests::update_refuses_when_the_sidecar_cannot_be_fetched` | 3 | 校验 sidecar 获取失败时 fail-closed，而非无校验更新。 |

共 **78 项**。除此之外的测试只有在补写出上述某一类别的具体守护性质后才可加入；不得用模块名、代码行数或“以后可能有用”代替测试级证据。
