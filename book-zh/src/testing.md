# 测试指南

## 快速开始

```bash
# 完整的本地 CI（与 GitHub Actions 一致）
./scripts/ci.sh

# 快速迭代（跳过 clippy）
./scripts/ci.sh --quick

# 自动修复格式
./scripts/ci.sh --fix

# 内存受限的机器
./scripts/ci.sh --serial
```

---

## CI 流水线

`scripts/ci.sh` 运行与 `.github/workflows/ci.yml` 相同的检查，外加针对性的子系统测试。

### 步骤

| 步骤 | 命令 | 标志 |
|------|---------|-------|
| 1. 格式化 | `cargo fmt --all -- --check` | `--fix` 自动修复 |
| 2. Clippy | `cargo clippy --workspace -- -D warnings` | `--quick` 跳过 |
| 3. 工作区测试 | `cargo test --workspace` | `--serial` 单线程 |
| 4. 针对性分组 | 按子系统测试（见下文） | 始终运行 |

### 针对性测试分组

在完整工作区测试之后，CI 脚本会单独重新运行关键子系统，以便清晰地暴露失败：

| 分组 | Crate | 测试过滤器 | 数量 | 覆盖内容 |
|-------|-------|-------------|-------|----------------|
| 自适应路由 | `octos-llm` | `adaptive::tests` | 19 | Off/Hedge/Lane 模式、熔断器、故障转移、评分、指标、竞速 |
| 响应性 | `octos-llm` | `responsiveness::tests` | 8 | 基线学习、劣化检测、恢复、阈值边界 |
| 会话 actor | `octos-cli` | `session_actor::tests` | 9 | 队列模式、Speculative 溢出、自动升级/降级 |
| 会话持久化 | `octos-bus` | `session::tests` | 28 | JSONL 存储、LRU 淘汰、分支、重写、时间戳排序 |

会话 actor 测试始终以单线程运行（`--test-threads=1`），因为它们会启动完整的 actor 和 mock 提供商，并行执行可能导致 OOM。

---

## 功能覆盖

### 自适应路由（`crates/octos-llm/src/adaptive.rs` — 19 个测试）

测试管理多个 LLM 提供商的 `AdaptiveRouter`，基于指标驱动选择。

#### Off 模式（静态优先级）

| 测试 | 验证内容 |
|------|-----------------|
| `test_selects_primary_on_cold_start` | 首次调用时的优先级顺序（尚无指标） |
| `test_lane_changing_off_uses_priority_order` | Off 模式忽略延迟差异 |
| `test_lane_changing_off_skips_circuit_broken` | Off 模式仍然遵守熔断器 |
| `test_hedged_off_uses_single_provider` | Off 模式使用优先级，不竞速 |

#### Hedge 模式（提供商竞速）

| 测试 | 验证内容 |
|------|-----------------|
| `test_hedged_racing_picks_faster_provider` | 通过 `tokio::select!` 竞速 2 个提供商，更快者胜出 |
| `test_hedged_racing_survives_one_failure` | 主竞速者失败时回退到备选 |
| `test_hedge_single_provider_falls_through` | 只有 1 个提供商时 Hedge 使用单提供商路径 |

#### Lane 模式（基于评分的选择）

| 测试 | 验证内容 |
|------|-----------------|
| `test_lane_mode_picks_best_by_score` | 指标预热后切换到更快的提供商 |

#### 熔断器与故障转移

| 测试 | 验证内容 |
|------|-----------------|
| `test_circuit_breaker_skips_degraded` | 连续 N 次失败后跳过提供商 |
| `test_failover_on_error` | 主提供商失败时转移到下一个 |
| `test_all_providers_fail` | 所有提供商都失败时返回错误 |

#### 评分与指标

| 测试 | 验证内容 |
|------|-----------------|
| `test_scoring_cold_start_respects_priority` | 冷启动评分遵循配置优先级 |
| `test_latency_samples_p95` | 从环形缓冲区计算 P95 |
| `test_metrics_snapshot` | 延迟/成功/失败正确记录 |
| `test_metrics_export_after_calls` | 导出包含按提供商的指标 |

#### 运行时控制

| 测试 | 验证内容 |
|------|-----------------|
| `test_mode_switch_at_runtime` | Off → Hedge → Lane → Off 切换 |
| `test_qos_ranking_toggle` | QoS 排名切换与模式正交 |
| `test_adaptive_status_reports_correctly` | 状态结构体反映当前模式/数量 |
| `test_empty_router_panics` | 断言至少需要 1 个提供商 |

### 响应性观察器（`crates/octos-llm/src/responsiveness.rs` — 8 个测试）

测试驱动自动升级的延迟跟踪器。

#### 基线学习

| 测试 | 验证内容 |
|------|-----------------|
| `test_baseline_learning` | 从前 5 个样本建立基线 |
| `test_sample_count_tracking` | `sample_count()` 返回正确值 |

#### 劣化检测

| 测试 | 验证内容 |
|------|-----------------|
| `test_degradation_detection` | 3 次连续慢请求（> 3 倍基线）触发激活 |
| `test_at_threshold_boundary_not_triggered` | 恰好在阈值处的延迟不视为"慢" |
| `test_no_false_trigger_before_baseline` | 基线建立前不会激活 |

#### 恢复与生命周期

| 测试 | 验证内容 |
|------|-----------------|
| `test_recovery_detection` | 激活后 1 次快速请求触发停用 |
| `test_multiple_activation_cycles` | 激活 → 停用 → 再激活正常工作 |
| `test_window_caps_at_max_size` | 滚动窗口保持在 20 条 |

### 队列模式与会话 Actor（`crates/octos-cli/src/session_actor.rs` — 9 个测试）

测试拥有消息处理、队列策略和自动保护的按会话 actor。

**Mock 基础设施：** `DelayedMockProvider` — 可配置延迟 + 脚本化 FIFO 响应。`setup_speculative_actor` / `setup_actor_with_mode` — 构建带有指定队列模式和可选自适应路由器的最小 actor。

#### 队列模式：Followup

| 测试 | 验证内容 |
|------|-----------------|
| `test_queue_mode_followup_sequential` | 每条消息独立处理 — 3 条消息产生 3 个响应，全部独立出现在会话历史中 |

#### 队列模式：Collect

| 测试 | 验证内容 |
|------|-----------------|
| `test_queue_mode_collect_batches` | 慢 LLM 调用期间排队的消息被批量合并为一个组合提示（`"msg2\n---\nQueued #1: msg3"`） |

#### 队列模式：Steer

| 测试 | 验证内容 |
|------|-----------------|
| `test_queue_mode_steer_keeps_newest` | 较旧的排队消息被丢弃，只处理最新的 — 被丢弃的消息不出现在会话历史中 |

#### 队列模式：Speculative

| 测试 | 验证内容 |
|------|-----------------|
| `test_speculative_overflow_concurrent` | 慢主 Agent 期间生成溢出作为完整 Agent 任务（12 秒 > 10 秒耐心值）；两个响应都到达；历史按时间戳排序 |
| `test_speculative_within_patience_drops` | 主 Agent 在耐心值内时溢出被丢弃（5 秒 < 10 秒）；只有 1 个响应到达 |
| `test_speculative_handles_background_result` | `BackgroundResult` 消息在 Speculative 的 `select!` 循环中被处理，不产生额外的 LLM 调用 |

#### 自动升级 / 降级

| 测试 | 验证内容 |
|------|-----------------|
| `test_auto_escalation_on_degradation` | 5 次快速预热（基线 100ms）→ 3 次慢调用（400ms > 3 倍）→ 模式切换为 Hedge + Speculative，用户收到通知 |
| `test_auto_deescalation_on_recovery` | 升级后 1 次快速响应 → 模式恢复为 Off + Followup，路由器确认 Off |

#### 工具函数

| 测试 | 验证内容 |
|------|-----------------|
| `test_strip_think_tags` | 从 LLM 输出中移除 `<think>...</think>` 块 |

### 会话持久化（`crates/octos-bus/src/session.rs` — 28 个测试）

测试基于 JSONL 的会话存储和 LRU 缓存。

#### CRUD 与持久化

| 测试 | 验证内容 |
|------|-----------------|
| `test_session_manager_create_and_retrieve` | 创建会话、添加消息、检索 |
| `test_session_manager_persistence` | 消息在管理器重启后存活（磁盘重载） |
| `test_session_manager_clear` | 清除从内存和磁盘中删除 |

#### 历史与排序

| 测试 | 验证内容 |
|------|-----------------|
| `test_session_get_history` | 尾部切片返回最后 N 条消息 |
| `test_session_get_history_all` | 不足最大值时返回全部 |
| `test_sort_by_timestamp_restores_order` | 并发溢出写入后恢复时间顺序 |

#### LRU 缓存

| 测试 | 验证内容 |
|------|-----------------|
| `test_eviction_keeps_max_sessions` | 缓存遵守容量限制 |
| `test_evicted_session_reloads_from_disk` | 被淘汰的会话访问时从磁盘重载 |
| `test_with_max_sessions_clamps_zero` | 容量下限钳制为 1 |

#### 并发

| 测试 | 验证内容 |
|------|-----------------|
| `test_concurrent_sessions` | 多个会话互不干扰 |
| `test_concurrent_session_processing` | 10 个并行任务不会损坏会话 |

#### 分支与重写

| 测试 | 验证内容 |
|------|-----------------|
| `test_fork_creates_child` | 分支复制最后 N 条消息并带有父链接 |
| `test_fork_persists_to_disk` | 分支的会话在重启后存活 |
| `test_session_rewrite` | 变更后的原子写入-重命名 |

#### 多会话（主题）

| 测试 | 验证内容 |
|------|-----------------|
| `test_list_sessions_for_chat` | 列出某个聊天的所有主题会话 |
| `test_session_topic_persists` | 主题在重启后存活 |
| `test_update_summary` | 摘要更新持久化 |
| `test_active_session_store` | 活跃主题切换和返回 |
| `test_active_session_store_persistence` | 活跃主题在重启后存活 |
| `test_validate_topic_name` | 拒绝无效字符和长度 |

#### 文件名编码

| 测试 | 验证内容 |
|------|-----------------|
| `test_truncated_session_keys_no_collision` | 带哈希后缀的长键不会冲突 |
| `test_decode_filename` | 百分号编码的文件名正确解码 |
| `test_list_sessions_returns_decoded_keys` | `list_sessions()` 返回人类可读的键 |
| `test_short_key_no_hash_suffix` | 短键不添加哈希后缀 |

#### 安全限制

| 测试 | 验证内容 |
|------|-----------------|
| `test_load_rejects_oversized_file` | 超过 10 MB 的文件被拒绝 |
| `test_append_respects_file_size_limit` | 文件达到 10 MB 限制时追加被跳过 |
| `test_load_rejects_future_schema_version` | 拒绝未知的 schema 版本 |
| `test_purge_stale_sessions` | 删除超过 N 天的会话 |

---

## 已知空白

| 领域 | 未测试原因 |
|------|---------------|
| **Interrupt 队列模式** | 与 Steer 共用代码路径 — 由 `test_queue_mode_steer_keeps_newest` 覆盖 |
| **探测/金丝雀请求** | 在所有测试中通过 `probe_probability: 0.0` 禁用以确保确定性 |
| **流式推送（`chat_stream`）** | 无 mock 流式基础设施；流式功能通过手动测试 |
| **会话压缩** | 在 actor 测试中调用但未验证输出（需要 LLM mock 进行摘要） |
| **实际提供商集成** | 需要 API 密钥；有 1 个测试但标记为 `#[ignore]` |
| **频道特定路由** | 由频道 crate 测试覆盖，不属于此子系统 |
| **"Earlier task" 标记** | 当溢出已响应时主响应添加 "Earlier task completed:" 前缀；测试中未直接断言（需要在慢主 + 快溢出竞速后检查出站内容） |
| **溢出 Agent 工具执行** | `serve_overflow` 启动完整的 `agent.process_message_tracked()` 并带有工具访问权限；当前测试使用 `DelayedMockProvider` 返回预设响应而不进行工具调用 |

---

## 运行单个测试

```bash
# 单个测试
cargo test -p octos-llm --lib adaptive::tests::test_hedged_racing_picks_faster_provider

# 一个子系统
cargo test -p octos-llm --lib adaptive::tests

# 会话 actor（始终单线程）
cargo test -p octos-cli session_actor::tests -- --test-threads=1

# 带输出
cargo test -p octos-cli session_actor::tests -- --test-threads=1 --nocapture
```

## GitHub Actions CI

`.github/workflows/ci.yml` 在 push/PR 到 `main` 时运行：

1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace -- -D warnings`
3. `cargo test --workspace`

本地的 `scripts/ci.sh` 是其超集 -- 除了运行相同的三个步骤外，还包含针对性的子系统测试组。如果本地 CI 通过，GitHub 上也会通过。

**运行器：** `macos-14`（ARM64）。私有仓库每月 2000 分钟免费额度（macOS runner 有 10 倍乘数 = 约 200 有效分钟）。

---

## 文件

| 文件 | 用途 |
|------|------|
| `scripts/ci.sh` | 本地 CI 脚本（本文档描述） |
| `scripts/pre-release.sh` | 完整的发布前冒烟测试（构建、端到端、技能二进制） |
| `.github/workflows/ci.yml` | GitHub Actions CI |
| `crates/octos-llm/src/adaptive.rs` | 自适应路由器 + 19 个测试 |
| `crates/octos-llm/src/responsiveness.rs` | 响应性观察器 + 8 个测试 |
| `crates/octos-cli/src/session_actor.rs` | 会话 actor + 9 个测试 |
| `crates/octos-bus/src/session.rs` | 会话持久化 + 28 个测试 |
