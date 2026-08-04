spec: task
name: "KV-Cache-Friendly Compaction"
tags: [agent-loop, compaction, kv-cache, performance]
estimate: 0.5d
---

## Intent

octos 的 agent loop 目前在回合中途做两类"重写前缀"的操作,导致前缀缓存
(KV cache)提供商(Kimi k3、DeepSeek)在每轮迭代重新 prefill 全量上下文:
(1) LRU 工具驱逐每轮迭代都可能改变请求最前部的 tools 数组(2026-08-02 实测:
kimi k3 回合第 5 轮 tools 47→32,之后每轮重刷约 36k token 输入);(2) Tier-1
微压缩每轮迭代就地改写历史深处的陈旧工具结果。本任务把这两类前缀失效收敛到
回合边界:回合内 tools 数组与既有历史保持字节稳定,仅允许清理"刚落地的超大
结果"(位于前缀尾部,缓存代价可忽略)。直接效果是降低 k3 等订阅制/按输入
计费模型的配额消耗与每轮首字延迟。

本合约的 v2.0.2 移植版同时携带:螺旋签名修复的测试(v2.0.2 将 loop_runner
测试外置为 loop_runner_tests.rs)与 Kimi Code 裸模型名缺口修复(openai.rs:
`k3-256k` 的 fixed_temperature 前缀匹配、裸 `k3`/`kimi-for-coding` 的
reasoning_content 存根)。

## Decisions

- 工具驱逐: v2.0.2 上游已整体移除 LRU 工具驱逐,回合内 tools 数组天然
  稳定,无需额外改动(原分支的 `should_auto_evict` 修复不再适用)。
- Tier-1 拆分: `MicroCompactionPolicy` 新增 `Tier1Pass` 枚举
  (`OversizedOnly` / `Full`)与 `prune_with_pass()`;每轮迭代只允许
  `OversizedOnly`(只清理超过 8KB 阈值的结果),`Full`(含 5 回合年龄阈值的
  陈旧清理)仅在回合首轮执行。既有 `prune()` 保持原签名并委托 `Full`,
  向后兼容既有调用与测试。
- 贯通路径: `TieredCompactionRunner::run_tier1` 与
  `run_tier1_compaction`(agent/compaction.rs)各新增一个 `Tier1Pass` 参数,
  由 loop_runner 两个调用点按 `iteration == 1` 选择档位。
- 不改变 Tier-3 全量压缩与 prompt_context_manager 通道的既有行为:
  Tier-3 的整体重写是可接受的一次性失效;有 context manager 时 Tier-1 本就
  早退,该早退保持不变。
- 保护名单语义不变: `protected_tool_call_ids` 在两档 pass 中均不可触碰。

## Boundaries

### Allowed Changes
- crates/octos-agent/src/agent/loop_runner.rs
- **/crates/octos-agent/src/agent/loop_runner.rs
- crates/octos-agent/src/agent/loop_runner_tests.rs
- **/crates/octos-agent/src/agent/loop_runner_tests.rs
- crates/octos-llm/src/openai.rs
- **/crates/octos-llm/src/openai.rs
- crates/octos-agent/src/agent/compaction.rs
- **/crates/octos-agent/src/agent/compaction.rs
- crates/octos-agent/src/compaction_tiered.rs
- **/crates/octos-agent/src/compaction_tiered.rs
- specs/kv-cache-friendly-compaction.spec.md
- **/specs/kv-cache-friendly-compaction.spec.md

### Forbidden
- 不要修改 `ToolRegistry::auto_evict()` 内部逻辑(spawn_only/base_tools
  不可驱逐等既有不变量由它自己维护)
- 不要改变 `prune()` 的公开签名或默认阈值常量
  (`DEFAULT_TIER1_MAX_AGE_TURNS`、`DEFAULT_TIER1_MAX_SIZE_BYTES_PER_RESULT`)
- 不要修改 Tier-2 (Anthropic context-editing) 与 Tier-3 (FullCompactor) 行为
- 不要修改 octos-cli 的 ContextManager / appui 压缩通道
- 不要添加新的 crate 依赖

## Out of Scope

- 按 provider 粒度的 cache-aware 开关(先让默认行为对所有 provider 更优)
- octos-cli ContextManager 通道的前缀稳定性验证(已由 2026-08-02 日志确认
  为追加式;如未来日志显示相反再立新约)
- Kimi 显式 context caching API 接入

## Completion Criteria

Scenario: OversizedOnly 档不触碰陈旧结果
  Test:
    Package: octos-agent
    Filter: oversized_only_pass_never_touches_stale_results
  Given 历史中存在一条超过年龄阈值但体积小的工具结果
  And 当前回合刚落地一条超过 8KB 的工具结果
  When 以 OversizedOnly 档执行 prune_with_pass
  Then 仅超大结果被替换为占位符
  And 陈旧但体积小的结果内容保持原文不变

Scenario: Full 档仍然清理陈旧结果
  Test:
    Package: octos-agent
    Filter: full_pass_still_prunes_stale_results
  Given 历史中存在一条超过年龄阈值的工具结果
  When 以 Full 档执行 prune_with_pass
  Then 该陈旧结果被替换为占位符

Scenario: 保护名单在 OversizedOnly 档同样生效
  Test:
    Package: octos-agent
    Filter: protected_ids_survive_the_oversized_only_pass
  Given 一条超过 8KB 的工具结果,其 tool_call_id 在保护名单中
  When 以 OversizedOnly 档执行 prune_with_pass
  Then 该结果内容保持原文不变
  And 本次 pass 报告零条清理
