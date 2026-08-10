# Deep Research Pipeline Timeout — Root Cause & Fix

## 问题

`run_pipeline` with `pipeline="deep_research"` 持续超时 (5 次, 1800s/900s), 无任何 artifact 产出。

## 根本原因

### 失败链条

```
search (fanout, 6 workers) → analyze (synthesize) → synthesize (report)
     ✅ 完成                      ❌ 卡死              ❌ 从未到达
```

### 失败点: analyze 节点

从 task ledger 日志:
```
"analyze (8 of 3) — failed"
"preview": "interrupted: node did not complete (error, cancellation, or panic)"
"variant": "budget_exhaustion"
"The agent did not complete within 30 iterations"
```

### 真正原因

**`max_iterations = 50` (默认值) 不够用**

`crates/octos-agent/src/agent/mod.rs:188`:
```rust
max_iterations: 50,  // 默认 50, 适合交互式对话
```

但 `analyze` 节点需要:
1. 读取 6 个 `findings-*.md` 文件 (每个可能几 KB)
2. 交叉引用/对比/分析
3. 输出结构化分析

**这需要 100–150 iterations, 不是 50。**

## 修复方案

### 方案 1: 为 pipeline workers 提高 max_iterations (推荐)

在 `deep_research.ir.json` 的 `analyze` 节点添加 `max_iterations` 配置:

```json
{
  "id": "analyze",
  "kind": {
    "type": "synthesize",
    "max_iterations": 150,  // ← 新增
    "prompt": "..."
  }
}
```

然后在 pipeline executor 中读取并应用:

`crates/octos-agent/src/executor.rs` (或 pipeline runner):
```rust
// 当 spawn analyze worker 时
let worker_config = AgentConfig {
    max_iterations: node.max_iterations.unwrap_or(150),  // pipeline workers 默认 150
    ..Default::default()
};
```

### 方案 2: 拆分 analyze 为多个小任务

把 "分析所有 findings" 拆成:
- analyze-1: 读取 findings-1.md, 提取关键点
- analyze-2: 读取 findings-2.md, 提取关键点
- ...
- synthesize: 合并所有 analyze 结果

**缺点**: 需要改 pipeline 结构, 更复杂。

### 方案 3: 临时 workaround (立即可用)

在 `delegate.rs` 中为 fanout workers 设置更高的默认 max_iterations:

`crates/octos-agent/src/tools/delegate.rs:374`:
```rust
pub fn with_worker_config(mut self, config: AgentConfig) -> Self {
    // Pipeline workers (fanout/synthesize) need more iterations than interactive chat
    let config = AgentConfig {
        max_iterations: 150,  // ← 从 50 改为 150
        ..config
    };
    self.worker_config = Some(config);
    self
}
```

## 建议

**短期**: 方案 3 (临时 workaround) — 立即修复, 不影响其他功能  
**长期**: 方案 1 (pipeline 配置化) — 更灵活, 每个节点可以自定义

## 验证

修复后测试:
```bash
# 应该能完成而不超时
run_pipeline pipeline="deep_research" input="..." timeout_secs=1800
```

## 相关文件

- `crates/octos-agent/src/agent/mod.rs:188` — 默认 max_iterations = 50
- `crates/octos-agent/src/assets/pipelines/deep_research.ir.json` — pipeline 定义
- `crates/octos-agent/src/tools/delegate.rs:374` — worker_config 设置点
- `crates/octos-agent/src/tools/spawn.rs:1814` — MAX_SPAWN_MAX_ITERATIONS = 300 (上限)

## 任务日志证据

```
session: dev:local:tui#coding
task: 019fe816-e8da-75a1-8a6e-2d9e30a2a861
error: "Tool 'run_pipeline' timed out after 1800s"
failed_node: "analyze (8 of 3)"
reason: "budget_exhaustion"
```
